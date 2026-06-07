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
//! - [`LivePlaylist`] — the HLS-0/HLS-1 rolling **live** driver (ADR-0032):
//!   windows + atomically publishes the `.m3u8` on each closed segment and prunes
//!   the evicted `.ts` from disk. The only part of this module that performs I/O
//!   (filesystem only — no native dependency).
//!
//! The CMAF segmenter and the blocking-reload HTTP origin server live behind
//! the off-by-default transport features; they feed segment/part metadata into
//! these builders. The pure-text builders here perform no I/O.
mod live;
mod master;
mod media;

pub use live::LivePlaylist;
pub use master::{MasterPlaylist, VariantStream};
pub use media::{MediaPlaylist, Part, Segment, SegmentType, ServerControl};
