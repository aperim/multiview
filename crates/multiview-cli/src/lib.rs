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

/// Build-time GPU-capability cross-check + health-warning emit (SA-0 / ADR-0035):
/// detect when a real GPU is present but the wgpu compositor resolved a
/// software/CPU adapter (the silent CPU fallback) and emit a latched, actionable
/// `gpu-present-no-vulkan-adapter` warning through the engine's drop-oldest
/// publisher. A thin seam over the pure hal cross-check + the control emit helper.
pub mod capability_warn;
pub mod cli;
/// Config-file watch (ADR-W020): hot-reload the impacted parts of the boot
/// config when the file changes externally — through the SAME apply machinery
/// the Web/API uses; an invalid file changes nothing (warn + health event).
pub mod config_watch;
pub mod control;
pub mod live_overlays;
pub mod live_sources;
/// The `multiview node` display-node support shell (DEV-B5 / ADR-0045): the
/// build-feature gate (`display-kms` + `ffmpeg`, clear errors otherwise) and
/// the load → validate → lower path from a node TOML to the runnable
/// `MultiviewConfig`. Always compiled, so the default build tests the
/// rejection path and a node build tests the acceptance path.
pub mod node;
/// The node enrollment client (DEV-B6 / ADR-0045 §9): a `multiview node`
/// becomes a managed `displaynode` device. The keypair + signing + request-body
/// core is always compiled and tested; the live HTTP enroll/heartbeat runner is
/// behind the off-by-default `node-enroll` feature. Additive — a node without a
/// `[controller]` block runs exactly as DEV-B5.
pub mod node_enroll;
/// Build-capability gating for configured outputs (DEV-B1 / ADR-0044): a
/// `display` output must FAIL a non-`display-kms` build with a clear error —
/// never be silently skipped. Always compiled, so the default build tests the
/// rejection path and a `display-kms` build tests the acceptance path.
pub mod outputs;
pub mod preview;
pub mod run;
/// Dependency-free systemd sd_notify (DEV-B5 / ADR-0045): READY/STOPPING/
/// STATUS + the tick-gated WATCHDOG over one non-blocking `AF_UNIX`
/// `SOCK_DGRAM` — best-effort, inert without `NOTIFY_SOCKET`.
pub mod sdnotify;
pub mod system_metrics;

/// Build-capability gating for `[timing].ptp_phc` (DEV-C1 / ADR-M010): a
/// configured PHC device must FAIL a non-`ptp` build at startup with a clear
/// error — never be silently downgraded to the system clock (the DEV-B1
/// fail-fast precedent). Always compiled, so the default build tests the
/// rejection path and a `ptp` build tests the acceptance path.
pub mod timing_gate;
/// The ~1 Hz outbound presentation-epoch publisher (DEV-C1 / ADR-M010): one
/// `WallClockRef` per program on the control WS (`timing.status`, conflated)
/// plus the shared HLS-PDT cell, derived off the hot path from the run's
/// tick-0 anchor and the disciplined wall clock. Never paces the tick loop
/// (invariant #1).
pub mod timing_status;
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

/// In-process **synthetic video sources** (ADR-0027): colour bars, a solid
/// colour, and a full-frame clock — rendered to NV12 and published into a
/// `TileStore` like any decoded feed. `bars`/`solid` build everywhere; the clock
/// renderer needs the `overlay` feature.
pub mod synth;

/// The full libav\* end-to-end `multiview run` pipeline (ingest → composite →
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
