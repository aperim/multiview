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
//! - [`rtsp`] — the OUT-1 RTSP-egress **sidecar baseline** seam (ADR-0006): the
//!   pure publish-URL builder ([`rtsp::RtspPublishTarget`]) that derives a libav
//!   RTSP ANNOUNCE/RECORD publish URL from a base + mount for the existing
//!   `PushProtocol::Rtsp` push path. No native dependency; the in-process
//!   `gst-rtsp-server` is OUT-2 (a separate feature).
//! - [`rtsp_server`] — the OUT-2 in-process RTSP **server** (ADR-0006 primary
//!   path): the always-compiled typed seam ([`rtsp_server::RtspServerSink`]
//!   [`PacketSink`](fanout::PacketSink) over a bounded drop-oldest
//!   [`rtsp_server::BoundedPacketQueue`], [`rtsp_server::RtspMount`], and
//!   [`rtsp_server::RtspCodec`] caps selection) plus, behind the off-by-default
//!   `rtsp-server` feature, the `gst-rtsp-server` serving thread that fans the
//!   already-encoded canvas to RTSP clients with no re-encode.
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
pub mod rtsp;
pub mod rtsp_server;
pub mod tsl;

/// Real encode-once-mux-many output sinks (file + HLS segmenter), built on
/// `multiview-ffmpeg`'s safe encoder/muxer wrappers. Behind the off-by-default
/// `ffmpeg` feature so the baseline build stays pure-Rust.
#[cfg(feature = "ffmpeg")]
pub mod sink;

/// GP-4 — the one-time pre-baked slate baker (ADR-0030 §4 "Pre-bake-once
/// slate"). Encodes **once** an IDR-led, closed-GOP, B-free loop of black /
/// SMPTE-bars video (+ optional AAC tone / silence) into a shared
/// [`slate::BakedSlate`] for a guarded passthrough to splice in on input loss,
/// then releases the encoder (no held session). Behind the off-by-default
/// `ffmpeg` feature; reuses [`sink::ProgramEncoder`] (no new FFI).
#[cfg(feature = "ffmpeg")]
pub mod slate;

/// Proprietary NDI® output (ADR-0008): the runtime-load scaffolding (locate +
/// `dlopen` the NDI runtime via `NDIlib_v6_load`), the runtime license gate, the
/// mandatory attribution constants, and the safe `NdiOutput` sink seam over the
/// resolved API table. Behind the off-by-default, license-isolating `ndi`
/// feature; the raw FFI lives in the `multiview-ndi-sys` leaf crate so this crate
/// stays `forbid(unsafe_code)`.
#[cfg(feature = "ndi")]
pub mod ndi;

pub use error::{Error, Result};
pub use rtsp::{RtspPublishError, RtspPublishTarget};
pub use rtsp_server::{
    units_to_nanos, BoundedPacketQueue, RtspCapsError, RtspCodec, RtspMount, RtspMountError,
    RtspServerSink,
};

#[cfg(feature = "ffmpeg")]
pub use sink::{
    AudioEncodeConfig, EncodeConfig, EncodeStats, FileSink, MuxStream, PacketMuxOutcome,
    PacketMuxSink, PacketSource, ProgramEncoder, PushProtocol, PushSink, SegmentResult,
    SegmentSink, VideoFrameSource,
};
