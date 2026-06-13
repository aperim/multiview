//! The `multiview` command-line grammar (clap derive).
//!
//! [`Cli`] is the top-level parser; [`Command`] is the subcommand union. Parsing
//! is pure and side-effect-free, so it is unit/integration-tested directly via
//! [`Cli::parse_from`] / [`clap::Parser::try_parse_from`] without spawning a
//! process.
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The `multiview` live video multiview engine — command-line interface.
///
/// The default build is pure-software (no GPU, no `FFmpeg`); hardware backends are
/// compiled in via the `nvidia` / `apple` / `linux-vaapi` / `full` feature
/// presets (see the crate manifest), not selected at runtime.
#[derive(Debug, Parser)]
#[command(name = "multiview", version, about, long_about = None)]
#[non_exhaustive]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    /// Parse arguments from an explicit iterator (the testable entrypoint).
    ///
    /// Panics on a usage error the same way [`clap::Parser::parse`] does; tests
    /// that want to assert on the error use [`clap::Parser::try_parse_from`].
    #[must_use]
    pub fn parse_from<I, T>(iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        <Self as Parser>::parse_from(iter)
    }
}

/// The `multiview` subcommands.
///
/// Intentionally **not** `#[non_exhaustive]`: this is the binary's dispatch
/// point, and the `match` over it must stay exhaustive so adding a subcommand is
/// a compile error until it is wired up.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Load a config, validate it (grid-solve + cross-references), and print a
    /// human-readable report. Pure and side-effect-free.
    Validate(ValidateArgs),
    /// Load + validate a config, build the engine, attach built-in test-pattern
    /// sources, and run. In `--software` mode this drives the FFmpeg-free output
    /// clock for `--ticks` ticks (or until Ctrl-C) and reports cadence/frames.
    Run(RunArgs),
    /// Run as a display node (ADR-0045): one supervised ingest (RTSP/SRT/HLS/
    /// MPEG-TS) → hardware decode → single-source full-canvas composite → the
    /// local DRM/KMS display head(s) (+ optional ALSA HDMI audio), reusing the
    /// unchanged ingest pacer/jitter/reconnect and the framestore tile ladder
    /// (last-good, then the configured local slate). Requires a build with the
    /// `display-kms` + `ffmpeg` features; fails with a clear error otherwise.
    Node(NodeArgs),
}

/// Arguments for `multiview validate`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct ValidateArgs {
    /// Path to the TOML configuration document to validate.
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,
}

/// Arguments for `multiview run`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct RunArgs {
    /// Path to the TOML configuration document to run.
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,

    /// Run the FFmpeg-free software engine (CPU reference compositor, built-in
    /// test-pattern sources) with no GPU or `FFmpeg` dependency. This is the
    /// software end-to-end smoke of the output-clock invariant — the same
    /// pipeline, minus the libav decoders, so it serves the API/WebUI and the
    /// program preview just like the full build, only without external ingest.
    ///
    /// Without this flag (and built with the `ffmpeg` feature), `run` builds the
    /// full libav* pipeline: ingest -> per-tile framestores -> the engine drive
    /// loop -> encode the canvas once -> fan out to the configured file/HLS
    /// outputs.
    ///
    /// `--headless` is accepted as a back-compat alias.
    #[arg(long, alias = "headless")]
    pub software: bool,

    /// Stop after this many output ticks (frames). Omit to run until Ctrl-C (or,
    /// for a bounded run, give `--duration` instead).
    #[arg(long, value_name = "N")]
    pub ticks: Option<u64>,

    /// Stop after this many seconds of output (converted to an exact whole
    /// number of ticks at the canvas cadence). Mutually informative with
    /// `--ticks`; if both are given, `--ticks` wins.
    #[arg(long, value_name = "SECS")]
    pub duration: Option<u64>,

    /// Burn an external SRT/`WebVTT` subtitle file into the program: the active
    /// cue is rendered (bottom-centre) on every output frame while it is on
    /// screen. Requires the `ffmpeg` + `overlay` features; ignored otherwise.
    /// The format is chosen by the file extension (`.vtt` ⇒ `WebVTT`, else SRT).
    #[arg(long, value_name = "FILE")]
    pub subtitles: Option<PathBuf>,

    /// Mux a **program-audio** elementary stream alongside the video (AUD-4): the
    /// output container gains a second (AAC) stream carrying the mixed program
    /// bus. Default OFF — without this flag the output is video-only and
    /// byte-identical to before. The program audio is silence until per-source
    /// audio decode is wired (a later slice), but it is a real AAC stream.
    /// Requires the `ffmpeg` feature; ignored otherwise.
    #[arg(long)]
    pub program_audio: bool,
}

impl RunArgs {
    /// Resolve the bounded tick budget from `--ticks` / `--duration` at the
    /// given canvas `cadence` (frames per second, exact rational).
    ///
    /// `--ticks` takes precedence; otherwise `--duration` seconds is converted
    /// to an exact whole number of ticks (`secs * num / den`, rounded toward
    /// zero). Returns [`None`] for an unbounded run (neither bound supplied).
    #[must_use]
    pub fn tick_budget(&self, cadence: multiview_core::time::Rational) -> Option<u64> {
        resolve_tick_budget(self.ticks, self.duration, cadence)
    }
}

/// Arguments for `multiview node` (ADR-0045 / DEV-B5).
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct NodeArgs {
    /// Path to the node TOML configuration document (one ingest, one or more
    /// display heads — see `multiview_config::node::NodeConfig`).
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,

    /// Stop after this many output ticks (frames) — a bounded diagnostic/soak
    /// run. Omit to run as the daemon (until Ctrl-C / SIGTERM).
    #[arg(long, value_name = "N")]
    pub ticks: Option<u64>,

    /// Stop after this many seconds of output (converted to an exact whole
    /// number of ticks at the canvas cadence). If both are given, `--ticks`
    /// wins.
    #[arg(long, value_name = "SECS")]
    pub duration: Option<u64>,
}

impl NodeArgs {
    /// Resolve the bounded tick budget from `--ticks` / `--duration` at the
    /// given canvas `cadence` — the same exact-integer math as
    /// [`RunArgs::tick_budget`] (never float fps).
    #[must_use]
    pub fn tick_budget(&self, cadence: multiview_core::time::Rational) -> Option<u64> {
        resolve_tick_budget(self.ticks, self.duration, cadence)
    }
}

/// The one tick-budget resolution shared by `run` and `node`: `--ticks` wins;
/// otherwise `--duration` seconds is converted to an exact whole number of
/// ticks (`secs * num / den`, i128 integer arithmetic — invariant #3, never a
/// float fps), rounded toward zero and clamped into `u64`. [`None`] = an
/// unbounded run.
fn resolve_tick_budget(
    ticks: Option<u64>,
    duration: Option<u64>,
    cadence: multiview_core::time::Rational,
) -> Option<u64> {
    if let Some(ticks) = ticks {
        return Some(ticks);
    }
    let secs = duration?;
    let num = i128::from(cadence.num);
    let den = i128::from(cadence.den).max(1);
    let budget = (i128::from(secs).saturating_mul(num)) / den;
    Some(u64::try_from(budget.max(0)).unwrap_or(u64::MAX))
}
