//! Generic stream decoders that yield frames mapped onto [`mosaic_core`] types.
//!
//! Where [`crate::decode::VideoDecoder`] is the minimal first-frame spike, the
//! decoders here are the pipeline-facing primitives: a [`StreamVideoDecoder`]
//! that pumps caller-supplied packets and yields NV12 host frames described by
//! [`mosaic_core::frame::FrameMeta`], and a [`StreamAudioDecoder`] that yields
//! raw audio frames. Both pair with [`crate::demux::Demuxer`] (which supplies
//! the packets) and own their decoder context (`Send + !Sync`, freed in `Drop`
//! by `ffmpeg_next`).
//!
//! ## NV12-throughout (invariant #5)
//! Software decoders typically emit planar `YUV420P`. [`StreamVideoDecoder`]
//! transparently converts that to NV12 via an internal [`crate::scale::Scaler`]
//! (libswscale), so every frame leaving this layer is on the canonical NV12
//! timeline. A frame already in NV12/P010 passes through untouched.
//!
//! ## Timestamps are input time (invariants #1/#3)
//! [`DecodedVideoFrame::meta`]'s `pts` is the frame's raw stream PTS rebased to
//! nanoseconds **through the stream time-base only** — it is still *input* time.
//! The engine applies cross-source rebasing and the output clock re-stamps from
//! the tick counter; nothing here is fed to a muxer.

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg_next as ffmpeg;

use mosaic_core::color::ColorInfo;
use mosaic_core::frame::FrameMeta;
use mosaic_core::pixel::PixelFormat;
use mosaic_core::time::{rescale, MediaTime, Rational};

use crate::convert::{color_from_ff, from_ff_rational};
use crate::error::{FfmpegError, Result};
use crate::scale::{ScaleSpec, Scaler};

/// One decoded video frame: the NV12 (or P010) host pixels plus its pure
/// [`FrameMeta`] description.
pub struct DecodedVideoFrame {
    /// The decoded pixels as a host [`Video`] frame in NV12 (or P010 for 10-bit
    /// sources). Borrow planes via [`Video::data`]/[`Video::stride`].
    pub frame: Video,
    /// The pure-Rust metadata describing this frame.
    pub meta: FrameMeta,
}

/// A video decoder that consumes caller-supplied packets and yields NV12 host
/// frames described by [`FrameMeta`].
pub struct StreamVideoDecoder {
    decoder: ffmpeg::decoder::Video,
    time_base: Rational,
    /// Lazily-built converter to NV12 (only when the decoder's output format is
    /// not already a canonical working format). Keyed implicitly by the source
    /// geometry/format it was built for; rebuilt on a mid-stream change.
    to_nv12: Option<Scaler>,
}

impl StreamVideoDecoder {
    /// Build a decoder from a [`Demuxer`](crate::demux::Demuxer) stream's
    /// parameters and time-base.
    ///
    /// # Errors
    /// Returns [`FfmpegError::OpenDecoder`] if a decoder cannot be built.
    pub fn new(parameters: ffmpeg::codec::Parameters, time_base: Rational) -> Result<Self> {
        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = ctx.decoder().video().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self {
            decoder,
            time_base,
            to_nv12: None,
        })
    }

    /// Send one coded packet to the decoder.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav send error.
    pub fn send_packet(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .map_err(FfmpegError::Decode)
    }

    /// Signal end-of-stream so buffered frames can be drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.decoder.send_eof().map_err(FfmpegError::Decode)
    }

    /// Pull the next decoded frame, converting to NV12 if needed.
    ///
    /// Returns `Ok(None)` when the decoder needs more input (`EAGAIN`) or is
    /// fully drained (`EOF`).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a real libav error, or
    /// [`FfmpegError::Convert`] if the NV12 conversion fails.
    pub fn receive_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
        let mut decoded = Video::empty();
        match self.decoder.receive_frame(&mut decoded) {
            Ok(()) => {}
            Err(
                ffmpeg::Error::Other {
                    errno: ffmpeg::util::error::EAGAIN,
                }
                | ffmpeg::Error::Eof,
            ) => return Ok(None),
            Err(other) => return Err(FfmpegError::Decode(other)),
        }

        let color = color_from_ff(
            decoded.color_space(),
            decoded.color_primaries(),
            decoded.color_transfer_characteristic(),
            decoded.color_range(),
        );
        let raw_pts = decoded.pts();
        let nv12 = self.ensure_nv12(decoded)?;

        // After `ensure_nv12` the frame is NV12 (8-bit) or P010LE (10-bit); any
        // other format is impossible, so it defaults to NV12-shaped metadata
        // rather than panicking on the hot path (CLAUDE.md §7).
        let mosaic_format = match nv12.format() {
            Pixel::P010LE => PixelFormat::P010,
            _ => PixelFormat::Nv12,
        };

        let pts = self.pts_to_media_time(raw_pts);
        let meta = FrameMeta {
            pts,
            width: nv12.width(),
            height: nv12.height(),
            format: mosaic_format,
            color,
        };
        Ok(Some(DecodedVideoFrame { frame: nv12, meta }))
    }

    /// Convert a decoded frame to NV12 if it is not already a canonical working
    /// format, rebuilding the converter when the source geometry/format changes.
    fn ensure_nv12(&mut self, decoded: Video) -> Result<Video> {
        match decoded.format() {
            // Already on the NV12-throughout timeline (or its 10-bit sibling).
            Pixel::NV12 | Pixel::P010LE => Ok(decoded),
            src_fmt => {
                let src = ScaleSpec::new(src_fmt, decoded.width(), decoded.height());
                let dst = ScaleSpec::new(Pixel::NV12, decoded.width(), decoded.height());
                let rebuild = match &self.to_nv12 {
                    Some(s) => s.source() != src || s.destination() != dst,
                    None => true,
                };
                if rebuild {
                    self.to_nv12 = Some(Scaler::new(src, dst)?);
                }
                let scaler = self.to_nv12.as_mut().ok_or(FfmpegError::FrameMismatch(
                    "NV12 scaler unexpectedly absent",
                ))?;
                scaler.run(&decoded)
            }
        }
    }

    /// Rebase a raw stream PTS into the internal nanosecond timeline using the
    /// stream time-base. An absent PTS maps to [`MediaTime::ZERO`].
    fn pts_to_media_time(&self, raw: Option<i64>) -> MediaTime {
        match raw {
            Some(ticks) => {
                let ns = rescale(ticks, self.time_base, Rational::new(1, 1_000_000_000));
                MediaTime::from_nanos(ns)
            }
            None => MediaTime::ZERO,
        }
    }
}

/// A decoded audio frame plus a minimal description.
pub struct DecodedAudioFrame {
    /// The decoded audio samples.
    pub frame: Audio,
    /// Presentation time on the internal nanosecond timeline (input time).
    pub pts: MediaTime,
}

/// An audio decoder consuming caller-supplied packets and yielding raw audio
/// frames (no resample here — that is the audio subsystem's job).
pub struct StreamAudioDecoder {
    decoder: ffmpeg::decoder::Audio,
    time_base: Rational,
}

impl StreamAudioDecoder {
    /// Build an audio decoder from a stream's parameters and time-base.
    ///
    /// # Errors
    /// Returns [`FfmpegError::OpenDecoder`] if a decoder cannot be built.
    pub fn new(parameters: ffmpeg::codec::Parameters, time_base: Rational) -> Result<Self> {
        let ctx = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = ctx.decoder().audio().map_err(FfmpegError::OpenDecoder)?;
        Ok(Self { decoder, time_base })
    }

    /// The decoder's sample rate in Hz.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.decoder.rate()
    }

    /// The decoder's channel count.
    #[must_use]
    pub fn channels(&self) -> u16 {
        self.decoder.channels()
    }

    /// Send one coded packet to the decoder.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav send error.
    pub fn send_packet(&mut self, packet: &ffmpeg::codec::packet::Packet) -> Result<()> {
        self.decoder
            .send_packet(packet)
            .map_err(FfmpegError::Decode)
    }

    /// Signal end-of-stream so buffered frames can be drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.decoder.send_eof().map_err(FfmpegError::Decode)
    }

    /// Pull the next decoded audio frame, or `Ok(None)` on `EAGAIN`/`EOF`.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a real libav error.
    pub fn receive_frame(&mut self) -> Result<Option<DecodedAudioFrame>> {
        let mut decoded = Audio::empty();
        match self.decoder.receive_frame(&mut decoded) {
            Ok(()) => {}
            Err(
                ffmpeg::Error::Other {
                    errno: ffmpeg::util::error::EAGAIN,
                }
                | ffmpeg::Error::Eof,
            ) => return Ok(None),
            Err(other) => return Err(FfmpegError::Decode(other)),
        }
        let pts = match decoded.pts() {
            Some(ticks) => MediaTime::from_nanos(rescale(
                ticks,
                self.time_base,
                Rational::new(1, 1_000_000_000),
            )),
            None => MediaTime::ZERO,
        };
        Ok(Some(DecodedAudioFrame {
            frame: decoded,
            pts,
        }))
    }
}

/// Convenience: the [`mosaic_core`] [`ColorInfo`] of a decoded frame after the
/// untagged-default policy is applied for its geometry.
///
/// Useful when a caller wants the *resolved* color (matrix/primaries inferred
/// from size) rather than the raw, possibly-`Unspecified` tags.
#[must_use]
pub fn resolved_color(meta: &FrameMeta) -> ColorInfo {
    meta.color.resolve_defaults(meta.width, meta.height)
}

/// The stream time-base helper used to convert this decoder's raw PTS values;
/// exposed for callers that want to rebase packets the same way.
#[must_use]
pub fn nanos_from_ticks(ticks: i64, time_base: ffmpeg::Rational) -> i64 {
    rescale(
        ticks,
        from_ff_rational(time_base),
        Rational::new(1, 1_000_000_000),
    )
}
