//! Safe demuxer over `libavformat` (the `ffmpeg` feature).
//!
//! [`Demuxer`] opens a URL/file, exposes each stream's parameters as a pure
//! [`StreamParams`] snapshot, reads coded packets, and seeks. It owns one
//! `AVFormatContext` (input) and frees it in `Drop` (via `ffmpeg_next`, which
//! owns the raw FFI). The context is `Send + !Sync` by construction.
//!
//! ## Timestamps are *input* timestamps
//! [`ReadPacket::pts`]/[`ReadPacket::dts`] are raw values in the stream's
//! time-base (carried alongside as [`StreamParams::time_base`]). They are
//! **input** timestamps — the engine rebases and the output clock re-stamps
//! everything (invariants #1/#3). Nothing read here is forwarded to a muxer
//! untouched.

use std::path::Path;

use ffmpeg::codec::Parameters;
use ffmpeg::media::Type;
use ffmpeg_next as ffmpeg;

use mosaic_core::time::Rational;

use crate::convert::{from_ff_rational, MediaKind};
use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};

/// A pure, owned snapshot of one stream's parameters.
///
/// Borrows nothing from the [`Demuxer`], so it can be stored, logged, or sent
/// across threads while the demuxer keeps reading.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StreamParams {
    /// Index of this stream within the container.
    pub index: usize,
    /// The media kind (video / audio / other).
    pub kind: MediaKind,
    /// The codec id name (e.g. `"h264"`, `"aac"`), as reported by libav.
    pub codec_name: String,
    /// The stream time-base (exact rational; **never** a float fps).
    pub time_base: Rational,
    /// Average frame rate, if the container declares one (video streams).
    pub avg_frame_rate: Option<Rational>,
    /// Video width in pixels (`0` for non-video / unknown).
    pub width: u32,
    /// Video height in pixels (`0` for non-video / unknown).
    pub height: u32,
    /// Audio sample rate in Hz (`0` for non-audio / unknown).
    pub sample_rate: u32,
    /// Audio channel count (`0` for non-audio / unknown).
    pub channels: u16,
    /// The stream's `language` metadata tag (BCP-47 / ISO 639 as declared by the
    /// container), if present. Used to resolve a caption rendition by language.
    pub language: Option<String>,
}

/// A read coded packet plus the index of the stream it belongs to.
///
/// The packet's `pts`/`dts` are raw stream-time-base values — input timestamps,
/// never forwarded to a muxer without rebasing/re-stamping.
pub struct ReadPacket {
    /// The stream index this packet belongs to.
    pub stream_index: usize,
    /// The underlying libav packet (ref-counted; freed on drop).
    pub packet: ffmpeg::codec::packet::Packet,
}

impl ReadPacket {
    /// Raw presentation timestamp in stream time-base ticks, if present.
    #[must_use]
    pub fn pts(&self) -> Option<i64> {
        self.packet.pts()
    }

    /// Raw decode timestamp in stream time-base ticks, if present.
    #[must_use]
    pub fn dts(&self) -> Option<i64> {
        self.packet.dts()
    }

    /// Packet payload size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.packet.size()
    }

    /// Whether the packet is flagged as a keyframe.
    #[must_use]
    pub fn is_key(&self) -> bool {
        self.packet.is_key()
    }
}

/// A safe demuxer bound to one opened input container.
///
/// Not `Sync`: the wrapped `AVFormatContext` requires external synchronization
/// for shared access (CLAUDE.md §7). It is `Send`, so it may move to the decode
/// thread that drives it.
pub struct Demuxer {
    input: ffmpeg::format::context::Input,
}

impl Demuxer {
    /// Open `path` as a media container, probing its streams.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::OpenInput`] — the container could not be opened/probed.
    pub fn open(path: &Path) -> Result<Self> {
        ensure_initialized()?;
        let input = ffmpeg::format::input(&path).map_err(|source| FfmpegError::OpenInput {
            path: path.display().to_string(),
            source,
        })?;
        Ok(Self { input })
    }

    /// Snapshot every stream's parameters.
    #[must_use]
    pub fn streams(&self) -> Vec<StreamParams> {
        self.input
            .streams()
            .map(|stream| {
                let params = stream.parameters();
                let kind = MediaKind::from(params.medium());
                let codec_name = codec_id_name(params.id());
                let mut p = StreamParams {
                    index: stream.index(),
                    kind,
                    codec_name,
                    time_base: from_ff_rational(stream.time_base()),
                    avg_frame_rate: rate_opt(stream.avg_frame_rate()),
                    width: 0,
                    height: 0,
                    sample_rate: 0,
                    channels: 0,
                    language: stream.metadata().get("language").map(str::to_owned),
                };
                // Decode-side geometry/audio params come from the codec context
                // built from the stream parameters; build a throwaway context to
                // read them without taking ownership of a decoder here.
                if let Ok(ctx) = ffmpeg::codec::context::Context::from_parameters(params) {
                    match kind {
                        MediaKind::Video => {
                            if let Ok(v) = ctx.decoder().video() {
                                p.width = v.width();
                                p.height = v.height();
                            }
                        }
                        MediaKind::Audio => {
                            if let Ok(a) = ctx.decoder().audio() {
                                p.sample_rate = a.rate();
                                p.channels = a.channels();
                            }
                        }
                        // Subtitle streams carry no decode-side geometry/audio to
                        // read here; the caption decoder reads what it needs from
                        // the stream parameters (`stream_parameters`).
                        MediaKind::Subtitle | MediaKind::Other => {}
                    }
                }
                p
            })
            .collect()
    }

    /// Index of the "best" stream of `kind`, per libav's heuristic.
    #[must_use]
    pub fn best_stream(&self, kind: MediaKind) -> Option<usize> {
        let ty = match kind {
            MediaKind::Video => Type::Video,
            MediaKind::Audio => Type::Audio,
            MediaKind::Subtitle => Type::Subtitle,
            MediaKind::Other => return None,
        };
        self.input.streams().best(ty).map(|s| s.index())
    }

    /// Clone the codec [`Parameters`] of the stream at `index`, or [`None`] if
    /// there is no such stream.
    ///
    /// This is what [`crate::caption_decode::CaptionDecoder::from_parameters`]
    /// consumes: a self-contained snapshot of the stream's codec parameters
    /// (codec id, extradata, geometry) that borrows nothing from the demuxer, so
    /// the caption decoder can be built on the input thread while the demuxer
    /// keeps reading.
    #[must_use]
    pub fn stream_parameters(&self, index: usize) -> Option<Parameters> {
        self.input.stream(index).map(|s| s.parameters())
    }

    /// Read the next coded packet from any stream, or [`None`] at end-of-stream.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a read error other than clean EOF.
    pub fn read_packet(&mut self) -> Result<Option<ReadPacket>> {
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut self.input) {
            Ok(()) => {
                let stream_index = packet.stream();
                Ok(Some(ReadPacket {
                    stream_index,
                    packet,
                }))
            }
            Err(ffmpeg::Error::Eof) => Ok(None),
            Err(other) => Err(FfmpegError::Decode(other)),
        }
    }

    /// Read the next packet belonging to `stream_index`, skipping others, or
    /// [`None`] at end-of-stream.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] for a read error other than clean EOF.
    pub fn read_packet_for(&mut self, stream_index: usize) -> Result<Option<ReadPacket>> {
        loop {
            match self.read_packet()? {
                Some(pkt) if pkt.stream_index == stream_index => return Ok(Some(pkt)),
                Some(_) => {}
                None => return Ok(None),
            }
        }
    }

    /// Seek the container to `timestamp` (in libav `AV_TIME_BASE` units,
    /// i.e. microseconds), landing on or before the target.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Decode`] if the seek fails.
    pub fn seek(&mut self, timestamp: i64) -> Result<()> {
        // `Input::seek` takes a target plus a min/max bracket; the full range
        // lets libav pick the nearest keyframe around the target.
        self.input.seek(timestamp, ..).map_err(FfmpegError::Decode)
    }
}

/// Human-readable codec id name (e.g. `"h264"`), or `"unknown"`.
fn codec_id_name(id: ffmpeg::codec::Id) -> String {
    // `Id` implements `Debug` as the libav constant name; the canonical short
    // name comes from the codec descriptor when available.
    ffmpeg::codec::decoder::find(id).map_or_else(
        || format!("{id:?}").to_ascii_lowercase(),
        |c| c.name().to_owned(),
    )
}

/// Treat libav's `0/0` / `0/1` "no rate" sentinels as absent.
fn rate_opt(rate: ffmpeg::Rational) -> Option<Rational> {
    if rate.numerator() == 0 {
        None
    } else {
        Some(from_ff_rational(rate))
    }
}
