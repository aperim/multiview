//! # multiview-output
//!
//! Output sinks/servers and packaging for the **Multiview** live video multiview
//! engine: the encode-once-mux-many fan-out model, HLS/LL-HLS playlist
//! generation, and (behind off-by-default features) the RTSP server, NDI out,
//! and RTMP/SRT push transports.
//!
//! This crate's **default build is pure Rust** with no native dependencies. The
//! two pure-Rust pillars are always available:
//!
//! - [`hls`] — HLS / LL-HLS playlist **text** generation (ADR-0007: Multiview
//!   builds the Apple Low-Latency tag layer in-house). No I/O, no `FFmpeg`.
//! - [`fanout`] — the encode-once-mux-many routing model (invariant #7): one
//!   encoded packet stream routed by reference to N transport sinks.
//! - [`tsl`] — TSL UMD (Under-Monitor Display) protocol **encoders** (v3.1 / v4.0
//!   / v5.0): pure typed label/tally messages → on-wire bytes, the egress mirror
//!   of `multiview-input::tsl`. Socket-free (a later integration) and off the engine
//!   hot path.
//!
//! The transports themselves (RTSP via `gst-rtsp-server`, the CMAF segmenter +
//! LL-HLS HTTP origin, NDI, RTMP/SRT) are feature-gated (`ffmpeg`, `ndi`, …) so
//! the GPU-free CI baseline never pulls a native library. The feature-gated
//! servers implement [`fanout::PacketSink`] and feed segment/part metadata into
//! [`hls`].
//!
//! ## Invariants this crate must uphold
//!
//! - **#7 encode-once-mux-many:** composite once, encode the canvas once per
//!   rendition, fan the *same* packets to all transports — never per-tile.
//! - **#3 timing:** packet timestamps are re-stamped from the tick counter
//!   upstream; this crate carries them as exact integers, never float fps.
//! - **#1/#10 isolation:** the fan-out and transport layer must never stall the
//!   output clock or let a slow client back-pressure the engine.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod fanout;
pub mod hls;
pub mod tsl;

/// Real encode-once-mux-many output sinks (file + HLS segmenter), built on
/// `multiview-ffmpeg`'s safe encoder/muxer wrappers. Behind the off-by-default
/// `ffmpeg` feature so the baseline build stays pure-Rust.
#[cfg(feature = "ffmpeg")]
pub mod sink;

pub use error::{Error, Result};

#[cfg(feature = "ffmpeg")]
pub use sink::{
    EncodeConfig, EncodeStats, FileSink, PacketMuxOutcome, PacketMuxSink, PacketSource,
    ProgramEncoder, PushProtocol, PushSink, SegmentResult, SegmentSink, VideoFrameSource,
};
