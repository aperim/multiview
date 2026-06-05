//! Safe encoder wrappers (the `ffmpeg` feature).
//!
//! [`VideoEncoder`] / [`AudioEncoder`] configure a codec from a target
//! description, then run the send-frame/receive-packet loop. Each owns its
//! codec context (`Send + !Sync`, freed in `Drop` by `ffmpeg_next`).
//!
//! ## Licensing (LGPL-clean default)
//! These wrappers configure whatever codec id the caller names; the **crate**
//! never defaults to a GPL encoder. Tests and the default pipeline use LGPL
//! software codecs already in `FFmpeg` (`mpeg2video`, `ffv1`, `mjpeg`); the GPL
//! `x264`/`x265` path is reserved for the separate `gpl-codecs` feature and is
//! never reachable through `ffmpeg` alone.
//!
//! ## Timestamps (invariants #1/#3)
//! The encoder's `time_base` is the caller's exact output cadence-derived
//! rational. Callers set each frame's PTS from the **tick counter** before
//! sending — raw input PTS is never forwarded. The receive side reports packet
//! PTS/DTS in encoder time-base for the muxer to rescale into stream time-base.

use ffmpeg::format::{Pixel, Sample};
use ffmpeg::util::frame::{Audio, Video};
use ffmpeg::{codec, encoder, ChannelLayout};
use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;

use crate::convert::to_ff_rational;
use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};

/// Target description for a [`VideoEncoder`].
#[derive(Debug, Clone)]
pub struct VideoEncodeTarget {
    /// libav short codec name (e.g. `"mpeg2video"`, `"ffv1"`, `"mjpeg"`). Must
    /// be an LGPL software codec for the default build.
    pub codec_name: String,
    /// Output frame width.
    pub width: u32,
    /// Output frame height.
    pub height: u32,
    /// Input/output pixel format fed to the encoder.
    pub format: Pixel,
    /// Encoder time-base — the exact output cadence reciprocal
    /// (e.g. `1001/60000` for 59.94 fps). Never a float fps.
    pub time_base: Rational,
    /// Target bitrate in bits/sec (`0` lets the codec choose / use quality).
    pub bit_rate: usize,
    /// Keyframe interval in frames (GOP size); `0` lets the codec choose.
    pub gop: u32,
}

/// A configured-and-opened video encoder.
pub struct VideoEncoder {
    encoder: encoder::video::Encoder,
    time_base: Rational,
}

impl VideoEncoder {
    /// Configure and open a video encoder for `target`.
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the named codec is not in this build.
    /// * [`FfmpegError::Rational`] — the time-base does not fit an `AVRational`.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new(target: &VideoEncodeTarget) -> Result<Self> {
        ensure_initialized()?;
        // Leak-safe: `codec_name` is matched against a static set of LGPL codecs
        // for the typed `CodecNotFound` message without allocating per-call.
        let codec = encoder::find_by_name(&target.codec_name)
            .ok_or_else(|| FfmpegError::CodecNotFound(static_codec_name(&target.codec_name)))?;

        let mut ctx = codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(FfmpegError::OpenEncoder)?;

        let tb = to_ff_rational(target.time_base)?;
        ctx.set_width(target.width);
        ctx.set_height(target.height);
        ctx.set_format(target.format);
        ctx.set_time_base(tb);
        if target.bit_rate > 0 {
            ctx.set_bit_rate(target.bit_rate);
        }
        if target.gop > 0 {
            ctx.set_gop(target.gop);
        }

        let encoder = ctx.open_as(codec).map_err(FfmpegError::OpenEncoder)?;
        Ok(Self {
            encoder,
            time_base: target.time_base,
        })
    }

    /// The encoder time-base (exact rational).
    #[must_use]
    pub const fn time_base(&self) -> Rational {
        self.time_base
    }

    /// Borrow the opened encoder's codec context — used to register a matching
    /// stream on a [`Muxer`](crate::mux::Muxer) (which copies codec parameters
    /// from it).
    #[must_use]
    pub fn as_codec_context(&self) -> &codec::Context {
        self.encoder.as_ref()
    }

    /// Snapshot the encoder's codec parameters into an owned, owner-less
    /// `AVCodecParameters` (`avcodec_parameters_from_context`). Used to build a
    /// [`StreamCodecParameters`](crate::packet::StreamCodecParameters) that
    /// crosses threads to a mux-only sink without the encoder instance
    /// (encode-once-mux-many, invariant #7).
    #[must_use]
    pub(crate) fn codec_parameters(&self) -> codec::Parameters {
        codec::Parameters::from(&self.encoder)
    }

    /// Send one frame, whose PTS the caller has already set from the tick
    /// counter (encoder time-base). Drain packets with [`Self::receive_packet`].
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav send error.
    pub fn send_frame(&mut self, frame: &Video) -> Result<()> {
        self.encoder.send_frame(frame).map_err(FfmpegError::Encode)
    }

    /// Flush the encoder (signal EOF) so buffered packets can be drained.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.encoder.send_eof().map_err(FfmpegError::Encode)
    }

    /// Pull the next encoded packet, or `Ok(None)` on `EAGAIN`/`EOF`.
    ///
    /// The returned packet's PTS/DTS are in encoder time-base; the muxer
    /// rescales them into stream time-base.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] for a real libav error.
    pub fn receive_packet(&mut self) -> Result<Option<codec::packet::Packet>> {
        receive_packet(&mut self.encoder)
    }
}

/// Target description for an [`AudioEncoder`].
#[derive(Debug, Clone)]
pub struct AudioEncodeTarget {
    /// libav short codec name (e.g. `"flac"`, `"pcm_s16le"`). LGPL only for the
    /// default build.
    pub codec_name: String,
    /// Sample format fed to the encoder.
    pub format: Sample,
    /// Channel layout.
    pub channel_layout: ChannelLayout,
    /// Sample rate in Hz (also the natural time-base reciprocal).
    pub sample_rate: u32,
    /// Target bitrate in bits/sec (`0` lets the codec choose).
    pub bit_rate: usize,
}

/// A configured-and-opened audio encoder.
pub struct AudioEncoder {
    encoder: encoder::audio::Encoder,
    time_base: Rational,
    frame_size: u32,
}

impl AudioEncoder {
    /// Configure and open an audio encoder for `target`.
    ///
    /// # Errors
    /// * [`FfmpegError::CodecNotFound`] — the named codec is not in this build.
    /// * [`FfmpegError::OpenEncoder`] — libav rejected the configuration.
    pub fn new(target: &AudioEncodeTarget) -> Result<Self> {
        ensure_initialized()?;
        let codec = encoder::find_by_name(&target.codec_name)
            .ok_or_else(|| FfmpegError::CodecNotFound(static_codec_name(&target.codec_name)))?;

        let mut ctx = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()
            .map_err(FfmpegError::OpenEncoder)?;

        // Audio time-base is 1/sample_rate.
        let time_base = Rational::new(1, i64::from(target.sample_rate));
        let tb = to_ff_rational(time_base)?;
        ctx.set_rate(i32::try_from(target.sample_rate).unwrap_or(i32::MAX));
        ctx.set_format(target.format);
        ctx.set_channel_layout(target.channel_layout);
        ctx.set_time_base(tb);
        if target.bit_rate > 0 {
            ctx.set_bit_rate(target.bit_rate);
        }

        let encoder = ctx.open_as(codec).map_err(FfmpegError::OpenEncoder)?;
        let frame_size = encoder.frame_size();
        Ok(Self {
            encoder,
            time_base,
            frame_size,
        })
    }

    /// The encoder time-base (`1/sample_rate`).
    #[must_use]
    pub const fn time_base(&self) -> Rational {
        self.time_base
    }

    /// The encoder's required samples-per-frame, or `0` if it accepts any
    /// (variable) frame size.
    #[must_use]
    pub const fn frame_size(&self) -> u32 {
        self.frame_size
    }

    /// Borrow the opened encoder's codec context — used to register a matching
    /// stream on a [`Muxer`](crate::mux::Muxer).
    #[must_use]
    pub fn as_codec_context(&self) -> &codec::Context {
        self.encoder.as_ref()
    }

    /// Snapshot the encoder's codec parameters into an owned, owner-less
    /// `AVCodecParameters`. See
    /// [`VideoEncoder::codec_parameters`](crate::encode::VideoEncoder::codec_parameters).
    #[must_use]
    pub(crate) fn codec_parameters(&self) -> codec::Parameters {
        codec::Parameters::from(&self.encoder)
    }

    /// Send one audio frame (PTS already set by the caller).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav send error.
    pub fn send_frame(&mut self, frame: &Audio) -> Result<()> {
        self.encoder.send_frame(frame).map_err(FfmpegError::Encode)
    }

    /// Flush the encoder (signal EOF).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] on a libav error.
    pub fn send_eof(&mut self) -> Result<()> {
        self.encoder.send_eof().map_err(FfmpegError::Encode)
    }

    /// Pull the next encoded packet, or `Ok(None)` on `EAGAIN`/`EOF`.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Encode`] for a real libav error.
    pub fn receive_packet(&mut self) -> Result<Option<codec::packet::Packet>> {
        receive_packet(&mut self.encoder)
    }
}

/// Shared receive-packet drain logic for both encoder kinds.
fn receive_packet<E>(encoder: &mut E) -> Result<Option<codec::packet::Packet>>
where
    E: ReceivePacket,
{
    let mut packet = codec::packet::Packet::empty();
    match encoder.recv(&mut packet) {
        Ok(()) => Ok(Some(packet)),
        Err(
            ffmpeg::Error::Other {
                errno: ffmpeg::util::error::EAGAIN,
            }
            | ffmpeg::Error::Eof,
        ) => Ok(None),
        Err(other) => Err(FfmpegError::Encode(other)),
    }
}

/// Internal abstraction over the two encoder types' `receive_packet`.
trait ReceivePacket {
    fn recv(
        &mut self,
        packet: &mut codec::packet::Packet,
    ) -> std::result::Result<(), ffmpeg::Error>;
}

impl ReceivePacket for encoder::video::Encoder {
    fn recv(
        &mut self,
        packet: &mut codec::packet::Packet,
    ) -> std::result::Result<(), ffmpeg::Error> {
        self.receive_packet(packet)
    }
}

impl ReceivePacket for encoder::audio::Encoder {
    fn recv(
        &mut self,
        packet: &mut codec::packet::Packet,
    ) -> std::result::Result<(), ffmpeg::Error> {
        self.receive_packet(packet)
    }
}

/// Map a runtime codec name to a `'static` string for the typed
/// [`FfmpegError::CodecNotFound`] message, covering the LGPL test/default
/// codecs; an unrecognized name falls back to a generic label.
fn static_codec_name(name: &str) -> &'static str {
    match name {
        "mpeg2video" => "mpeg2video",
        "ffv1" => "ffv1",
        "mjpeg" => "mjpeg",
        "rawvideo" => "rawvideo",
        "flac" => "flac",
        "pcm_s16le" => "pcm_s16le",
        "aac" => "aac",
        "libopus" => "libopus",
        "mp2" => "mp2",
        _ => "<encoder>",
    }
}
