//! HLS / LL-HLS playlist generation (pure text).
//!
//! Per ADR-0007, Multiview is CMAF-first and builds the Apple Low-Latency HLS tag
//! layer in-house (`FFmpeg`'s `hls` muxer cannot emit it). This module owns the
//! **text generation** only:
//!
//! - [`MediaPlaylist`] — a media playlist with a sliding window, discontinuity
//!   handling, and the full LL-HLS tag set (`EXT-X-PART`, `EXT-X-PART-INF`,
//!   `EXT-X-SERVER-CONTROL`, `EXT-X-PRELOAD-HINT`, `EXT-X-RENDITION-REPORT`).
//! - [`MasterPlaylist`] — a multivariant playlist of [`VariantStream`]s.
//!
//! The CMAF segmenter and the blocking-reload HTTP origin server live behind
//! the off-by-default transport features; they feed segment/part metadata into
//! these builders. Nothing here performs I/O or pulls a native dependency.
mod master;
mod media;

pub use master::{MasterPlaylist, VariantStream};
pub use media::{MediaPlaylist, Part, Segment, SegmentType, ServerControl};
