//! Minimal, safe demux + video-decode path over libav* (the `ffmpeg` feature).
//!
//! This is the first vertical slice proving the chosen libav* binding
//! ([`ffmpeg_next`], which links the **system** libav* via `pkg-config` and
//! auto-detects the installed `FFmpeg` version at build time) compiles and runs
//! against the host `FFmpeg`. It opens a container, finds the best video stream,
//! constructs a software decoder, pumps packets, and yields the first decoded
//! frame's geometry / pixel format / presentation timestamp.
//!
//! ## Scope and invariants
//! * **No raw FFI here.** `ffmpeg_next` owns the `unsafe` boundary; this module
//!   is ordinary safe Rust calling its wrappers, so the crate's
//!   `unsafe_code = "deny"` is upheld with zero `unsafe` blocks on this path.
//! * **`!Sync` by construction.** [`VideoDecoder`] holds libav contexts that
//!   must not be shared across threads unsynchronized (CLAUDE.md §7); it is
//!   `Send` but deliberately not `Sync` because it owns `&mut`-style state and
//!   exposes no interior mutability.
//! * **PTS are *input* timestamps.** [`DecodedFrameInfo::pts`] is the raw,
//!   per-input presentation timestamp in stream time-base ticks. The engine's
//!   output clock re-stamps everything from the tick counter (invariant #1/#3);
//!   nothing here is ever fed to a muxer.

use std::path::Path;
use std::sync::Once;

use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};

/// Guards one-time global libav* initialization.
static INIT: Once = Once::new();

/// Run libav*'s global initialization exactly once for the process.
///
/// `ffmpeg_next::init()` registers the demuxers/decoders and is idempotent at
/// the libav level; the [`Once`] simply avoids redundant calls. The first
/// caller observes any failure; subsequent callers assume success (libav has
/// no way to re-report it, and a failed registration is fatal regardless).
///
/// # Errors
/// Returns [`FfmpegError::Init`] if libav initialization fails.
pub fn ensure_initialized() -> Result<()> {
    let mut outcome: Result<()> = Ok(());
    INIT.call_once(|| {
        if let Err(err) = ffmpeg::init() {
            outcome = Err(FfmpegError::Init(err));
        }
    });
    outcome
}

/// Geometry, pixel format, and timing of a single decoded video frame.
///
/// This is a plain owned snapshot — it borrows nothing from the decoder, so it
/// can outlive the [`VideoDecoder`] and cross thread/channel boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodedFrameInfo {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// libav pixel format of the decoded frame (e.g. `Pixel::YUV420P`).
    pub format: Pixel,
    /// Raw presentation timestamp in *stream time-base ticks*, if the frame
    /// carries one. This is an **input** timestamp — never forward it to an
    /// encoder/muxer; the output clock re-stamps from the tick counter.
    pub pts: Option<i64>,
}

/// A software video decoder bound to one stream of an opened input container.
///
/// Constructed via [`VideoDecoder::open`]. Holds the demuxer (`input` context)
/// and the decoder context together so packets can be pumped without exposing
/// any raw libav pointer. Not `Sync`: libav contexts require external
/// synchronization for shared access (CLAUDE.md §7).
pub struct VideoDecoder {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
}

impl VideoDecoder {
    /// Open `path` as a media container and build a software video decoder for
    /// its best video stream.
    ///
    /// Performs one-time libav initialization, opens and probes the container,
    /// selects the best video stream, and constructs a decoder from that
    /// stream's codec parameters.
    ///
    /// # Errors
    /// * [`FfmpegError::Init`] — global libav init failed.
    /// * [`FfmpegError::OpenInput`] — the container could not be opened/probed.
    /// * [`FfmpegError::StreamNotFound`] — no video stream is present.
    /// * [`FfmpegError::OpenDecoder`] — a decoder could not be built for the
    ///   selected stream.
    pub fn open(path: &Path) -> Result<Self> {
        ensure_initialized()?;

        let input = ffmpeg::format::input(&path).map_err(|source| FfmpegError::OpenInput {
            path: path.display().to_string(),
            source,
        })?;

        let (stream_index, parameters) = {
            let stream = input
                .streams()
                .best(Type::Video)
                .ok_or(FfmpegError::StreamNotFound("video"))?;
            (stream.index(), stream.parameters())
        };

        let codec_context = ffmpeg::codec::context::Context::from_parameters(parameters)
            .map_err(FfmpegError::OpenDecoder)?;
        let decoder = codec_context
            .decoder()
            .video()
            .map_err(FfmpegError::OpenDecoder)?;

        Ok(Self {
            input,
            decoder,
            stream_index,
        })
    }

    /// The index of the video stream this decoder is bound to.
    #[must_use]
    pub fn stream_index(&self) -> usize {
        self.stream_index
    }

    /// Demux and decode until the first video frame is produced, returning its
    /// geometry / format / PTS.
    ///
    /// Pumps packets from the bound video stream into the decoder, draining
    /// frames after each packet, then flushes the decoder at end-of-stream.
    /// `EAGAIN` (decoder needs more input) is handled transparently.
    ///
    /// # Errors
    /// * [`FfmpegError::Decode`] — a non-drain libav error while sending a
    ///   packet or receiving a frame.
    /// * [`FfmpegError::EndOfStream`] — the input ended before any video frame
    ///   could be decoded.
    pub fn decode_first_frame(&mut self) -> Result<DecodedFrameInfo> {
        let mut frame = ffmpeg::frame::Video::empty();

        // Snapshot packets up front: `packets()` borrows the input mutably, so
        // we cannot hold that iterator while also calling `self`-methods.
        let packets: Vec<(usize, ffmpeg::codec::packet::Packet)> = self
            .input
            .packets()
            .filter_map(|(stream, packet)| {
                if stream.index() == self.stream_index {
                    Some((stream.index(), packet))
                } else {
                    None
                }
            })
            .collect();

        for (_, packet) in &packets {
            self.decoder
                .send_packet(packet)
                .map_err(FfmpegError::Decode)?;
            if let Some(info) = Self::drain_one(&mut self.decoder, &mut frame)? {
                return Ok(info);
            }
        }

        // Flush: signal EOF and drain whatever the decoder buffered.
        self.decoder.send_eof().map_err(FfmpegError::Decode)?;
        if let Some(info) = Self::drain_one(&mut self.decoder, &mut frame)? {
            return Ok(info);
        }

        Err(FfmpegError::EndOfStream("video"))
    }

    /// Try to receive exactly one frame from the decoder.
    ///
    /// Returns `Ok(Some(info))` if a frame was produced, `Ok(None)` if the
    /// decoder reported `EAGAIN`/`EOF` (needs more input / fully drained), or
    /// an error for any other libav failure.
    fn drain_one(
        decoder: &mut ffmpeg::decoder::Video,
        frame: &mut ffmpeg::frame::Video,
    ) -> Result<Option<DecodedFrameInfo>> {
        match decoder.receive_frame(frame) {
            Ok(()) => Ok(Some(DecodedFrameInfo {
                width: frame.width(),
                height: frame.height(),
                format: frame.format(),
                pts: frame.pts(),
            })),
            // `EAGAIN` (more input needed) and `Eof` (fully drained) are normal
            // control-flow signals, not failures.
            Err(
                ffmpeg::Error::Other {
                    errno: ffmpeg::util::error::EAGAIN,
                }
                | ffmpeg::Error::Eof,
            ) => Ok(None),
            Err(other) => Err(FfmpegError::Decode(other)),
        }
    }
}

// `VideoDecoder` owns libav contexts that must not be shared across threads
// without synchronization; it is `Send` (it may move to a decode thread) but
// intentionally `!Sync`. `ffmpeg_next`'s context types are already `Send` and
// not `Sync`, so no manual marker impls are needed — this is asserted in tests.
