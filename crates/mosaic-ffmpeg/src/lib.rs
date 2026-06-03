//! # mosaic-ffmpeg
//!
//! Safe RAII wrappers over libav\* (demux/decode/encode, hardware-frame
//! lifecycle). All raw libav\* FFI is owned by this crate and lives **behind
//! the off-by-default `ffmpeg` feature**, so the default workspace build is
//! pure-Rust with no native dependencies. The library target is
//! `mosaic_ffmpeg`.
//!
//! ## Binding
//!
//! The `ffmpeg` feature links the **system** libav\* through
//! [`ffmpeg_next`](https://docs.rs/ffmpeg-next), which discovers the libraries
//! via `pkg-config` and auto-detects the installed `FFmpeg` version at build
//! time (no per-version Cargo feature required). It is validated against
//! `FFmpeg` 7.1 / libavcodec 61. `ffmpeg_next` provides the `unsafe` boundary
//! for the demux/decode/encode/mux/scale/resample wrappers, which are therefore
//! ordinary safe Rust. The **only** raw FFI in the crate is the hardware-frame
//! lifecycle scaffold in `hwframe`; the crate compiles under
//! `unsafe_code = "deny"` (not `forbid`), and every `unsafe` block there carries
//! a `// SAFETY:` comment stating the invariant it upholds (CLAUDE.md §7).
//!
//! ## Module map
//!
//! The modules below are compiled behind the off-by-default `ffmpeg` feature, so
//! they are named as plain code spans (they are absent from the default,
//! pure-Rust doc build).
//!
//! * `convert` — pure value mappings between libav and [`mosaic_core`] types.
//! * `demux` — `Demuxer`: open a container, list streams, read packets, seek.
//! * `decode` — `VideoDecoder`: minimal first-frame spike.
//! * `decode_stream` — `StreamVideoDecoder`/`StreamAudioDecoder`: packet-fed
//!   decoders yielding NV12 host frames + [`mosaic_core::frame::FrameMeta`].
//! * `audio_file` — `AudioFileDecoder`: file → interleaved-`f32` blocks
//!   (`AudioSamplesF32`) with no libav type in its public API.
//! * `scale` / `resample` — libswscale / libswresample wrappers.
//! * `encode` — `VideoEncoder`/`AudioEncoder` configured from a target.
//! * `mux` — `Muxer`: write a container, re-stamping packet timestamps.
//! * `hwframe` — `AVHWFramesContext`/`AVHWDeviceContext` RAII scaffold.
//!
//! One further module is **always compiled** (its decision logic is pure):
//!
//! * [`jpegxs`] — ST 2110-22 JPEG XS codec identity + capability detection. The
//!   path-selection algorithm and value types are native-dep-free and unit
//!   tested; only the libav-backed `probe`/`is_available` runtime queries are
//!   behind the `ffmpeg` feature.
//!
//! ## Licensing
//!
//! The default build stays LGPL-clean: only LGPL software codecs are used.
//! GPL software encoders (x264/x265) are reserved for the separate
//! `gpl-codecs` feature and are never pulled in by `ffmpeg` alone. Test and
//! default encodes use in-tree LGPL software codecs (`mpeg2video`, `ffv1`,
//! `mjpeg`, `rawvideo`).
//!
//! See [`docs/research/core-engine.md`](../docs/research/core-engine.md) §7,
//! §8.1, §12 for the FFI / hardware-frame design this crate builds toward.

/// The unified **caption cue** model (`CaptionCue` text/bitmap shapes, regions,
/// rects) plus the pure ASS-event markup stripper. Always compiled and
/// native-dep-free; the feature-gated [`caption_decode`] decoders emit these
/// types. See [`docs/io/captions.md`](../docs/io/captions.md) §1.
pub mod caption;

/// Video-codec identity + encoder selection. The logical [`codec::VideoCodec`]
/// enum and the feature-gated candidate-list logic are pure (always compiled,
/// unit-tested); only the libav-backed `codec::select_encoder` run-time probe
/// lives behind the `ffmpeg` feature.
pub mod codec;

pub mod error;

/// ST 2110-22 JPEG XS codec identity + capability detection. The selection
/// algorithm and value types are pure (always compiled, unit-tested); the
/// libav-backed `probe`/`is_available` runtime queries live behind the
/// `ffmpeg` feature.
pub mod jpegxs;

pub use caption::{
    strip_ass_event, CaptionCue, CueAnchor, CueBitmap, CueError, CueRect, CueRegion, CueText,
};

pub use codec::{can_encode, candidate_encoders, VideoCodec};

#[cfg(feature = "ffmpeg")]
pub use codec::select_encoder;

pub use error::{FfmpegError, Result};

pub use jpegxs::{
    resolve_availability, select_codec_name, JpegXsAvailability, JpegXsRole, JPEGXS_CODEC_NAMES,
};

#[cfg(feature = "ffmpeg")]
pub use jpegxs::{is_available as jpegxs_is_available, probe as jpegxs_probe};

#[cfg(feature = "ffmpeg")]
pub mod audio_file;

/// Safe RAII caption decoders over the linked libav subtitle decoders, emitting
/// the unified [`caption::CaptionCue`] model. Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod caption_decode;

#[cfg(feature = "ffmpeg")]
pub mod convert;

#[cfg(feature = "ffmpeg")]
pub mod decode;

#[cfg(feature = "ffmpeg")]
pub mod decode_stream;

#[cfg(feature = "ffmpeg")]
pub mod demux;

#[cfg(feature = "ffmpeg")]
pub mod encode;

#[cfg(feature = "ffmpeg")]
pub mod hwframe;

#[cfg(feature = "ffmpeg")]
pub mod mux;

#[cfg(feature = "ffmpeg")]
pub mod resample;

#[cfg(feature = "ffmpeg")]
pub mod scale;

#[cfg(feature = "ffmpeg")]
pub use audio_file::{AudioFileDecoder, AudioSamplesF32};

#[cfg(feature = "ffmpeg")]
pub use caption_decode::{CaptionDecoder, CaptionSource, CcChannel};

#[cfg(feature = "ffmpeg")]
pub use convert::{
    color_from_ff, from_ff_rational, pixel_to_ff, pixel_to_mosaic, to_ff_rational, MediaKind,
};

#[cfg(feature = "ffmpeg")]
pub use decode::{ensure_initialized, DecodedFrameInfo, VideoDecoder};

#[cfg(feature = "ffmpeg")]
pub use decode_stream::{
    DecodedAudioFrame, DecodedVideoFrame, StreamAudioDecoder, StreamVideoDecoder,
};

#[cfg(feature = "ffmpeg")]
pub use demux::{Demuxer, ReadPacket, StreamParams};

#[cfg(feature = "ffmpeg")]
pub use encode::{AudioEncodeTarget, AudioEncoder, VideoEncodeTarget, VideoEncoder};

#[cfg(feature = "ffmpeg")]
pub use hwframe::{HwDeviceContext, HwDeviceKind, HwFramesContext, HwFramesSpec};

#[cfg(feature = "ffmpeg")]
pub use mux::Muxer;

#[cfg(feature = "ffmpeg")]
pub use resample::{ResampleSpec, Resampler};

#[cfg(feature = "ffmpeg")]
pub use scale::{ScaleSpec, Scaler};
