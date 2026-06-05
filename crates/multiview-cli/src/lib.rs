//! # multiview-cli
//!
//! The library half of the `multiview` binary: the argument grammar
//! ([`cli`]), the `validate` subcommand ([`validate`]), the `run`
//! subcommand's headless software engine ([`run`]), and — behind the
//! off-by-default `ffmpeg` feature — the real libav\* end-to-end pipeline
//! (`pipeline`: ingest → composite → encode-once → fan out to file/HLS sinks).
//!
//! The binary ([`crate`]'s `main.rs`) is a thin shell that parses
//! [`cli::Cli`] and dispatches to these modules. Keeping the logic in a library
//! target lets the integration tests under `tests/` exercise the *real* code
//! paths — config validation and the deterministic headless engine — with no
//! process spawning.
//!
//! ## What `run --headless` proves
//!
//! `run --headless` is a pure-software, GPU-free, FFmpeg-free end-to-end smoke
//! of **invariant #1 (output-clock)**: it builds the protected output core
//! ([`multiview_engine`]) wired from a validated layout — the fixed-cadence
//! [`multiview_engine::OutputClock`], the CPU reference [`multiview_engine::CompositorDrive`],
//! and one [`multiview_framestore::TileStore`] per source — attaches built-in
//! test-pattern sources that publish synthetic NV12 frames into those stores,
//! and drives exactly one composited frame per tick for the requested number of
//! ticks. It reports frames produced, the measured cadence, and that the output
//! never faltered (frames == ticks, monotone PTS).
//!
//! ## Isolation (invariant #10)
//!
//! The headless engine consumes engine state through the engine's own outbound
//! [`multiview_engine::EnginePublisher`] (the wait-free latest-state slot + the
//! drop-oldest broadcast). The CLI is a best-effort observer: it never holds a
//! lock the engine needs and never makes the engine `.await` it.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cli;
pub mod control;
pub mod run;
pub mod validate;

/// The overlay draw-data baker (feature `overlay`): builds the per-frame overlay
/// primitives — clock label, dB meter, safe-area, tally, burned-in subtitles —
/// that the run paths bake into the composited program off the hot path.
#[cfg(feature = "overlay")]
pub mod overlays;

/// The **real wall-clock time-of-day source** for the on-screen clock overlay
/// (feature `overlay`): reads the OS `CLOCK_REALTIME` via `std` (no NTP
/// reimplementation), exposes an injectable [`wallclock::WallClock`] seam for
/// tests, and carries the [`multiview_overlay::clock::TimeRef`] reference badge. The
/// displayed time-of-day is sampled live at bake time (anti-drift), never derived
/// from the output-tick counter — the engine's output cadence stays untouched.
#[cfg(feature = "overlay")]
pub mod wallclock;

/// The **real** libav\* end-to-end `multiview run` pipeline (ingest → composite →
/// encode-once → fan out to file/HLS sinks). Behind the off-by-default `ffmpeg`
/// feature so the baseline build stays pure-Rust; software H.264/H.265 needs
/// `gpl-codecs` on top.
#[cfg(feature = "ffmpeg")]
pub mod pipeline;

/// Native in-pipeline **HLS WebVTT caption ingest**: resolve a source's subtitle
/// rendition from its HLS master playlist, demux + decode it on an isolated
/// reader thread, and publish cues into a per-source store the overlay baker
/// samples per output tick (per-tile burn-in). Behind the `ffmpeg` feature.
#[cfg(feature = "ffmpeg")]
pub mod captions;
