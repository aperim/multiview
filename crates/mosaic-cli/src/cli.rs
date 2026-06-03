//! The `mosaic` command-line grammar (clap derive).
//!
//! [`Cli`] is the top-level parser; [`Command`] is the subcommand union. Parsing
//! is pure and side-effect-free, so it is unit/integration-tested directly via
//! [`Cli::parse_from`] / [`clap::Parser::try_parse_from`] without spawning a
//! process.
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// The `mosaic` live video mosaic engine — command-line interface.
///
/// The default build is pure-software (no GPU, no `FFmpeg`); hardware backends are
/// compiled in via the `nvidia` / `apple` / `linux-vaapi` / `full` feature
/// presets (see the crate manifest), not selected at runtime.
#[derive(Debug, Parser)]
#[command(name = "mosaic", version, about, long_about = None)]
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

/// The `mosaic` subcommands.
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
    /// sources, and run. In `--headless` mode this drives the software output
    /// clock for `--ticks` ticks (or until Ctrl-C) and reports cadence/frames.
    Run(RunArgs),
}

/// Arguments for `mosaic validate`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct ValidateArgs {
    /// Path to the TOML configuration document to validate.
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,
}

/// Arguments for `mosaic run`.
#[derive(Debug, Args)]
#[non_exhaustive]
pub struct RunArgs {
    /// Path to the TOML configuration document to run.
    #[arg(value_name = "CONFIG")]
    pub config: PathBuf,

    /// Run the pure-software engine (CPU reference compositor, built-in
    /// test-pattern sources) with no GPU or `FFmpeg` dependency. This is the
    /// software end-to-end smoke of the output-clock invariant.
    ///
    /// Without this flag (and built with the `ffmpeg` feature), `run` builds the
    /// real libav* pipeline: ingest -> per-tile framestores -> the engine drive
    /// loop -> encode the canvas once -> fan out to the configured file/HLS
    /// outputs.
    #[arg(long)]
    pub headless: bool,

    /// Stop after this many output ticks (frames). Omit to run until Ctrl-C (or,
    /// for a bounded run, give `--duration` instead).
    #[arg(long, value_name = "N")]
    pub ticks: Option<u64>,

    /// Stop after this many seconds of output (converted to an exact whole
    /// number of ticks at the canvas cadence). Mutually informative with
    /// `--ticks`; if both are given, `--ticks` wins.
    #[arg(long, value_name = "SECS")]
    pub duration: Option<u64>,
}

impl RunArgs {
    /// Resolve the bounded tick budget from `--ticks` / `--duration` at the
    /// given canvas `cadence` (frames per second, exact rational).
    ///
    /// `--ticks` takes precedence; otherwise `--duration` seconds is converted
    /// to an exact whole number of ticks (`secs * num / den`, rounded toward
    /// zero). Returns [`None`] for an unbounded run (neither bound supplied).
    #[must_use]
    pub fn tick_budget(&self, cadence: mosaic_core::time::Rational) -> Option<u64> {
        if let Some(ticks) = self.ticks {
            return Some(ticks);
        }
        let secs = self.duration?;
        // ticks = secs * fps = secs * num / den, exact integer arithmetic on
        // i128 to avoid any float fps (invariant #3), clamped into u64.
        let num = i128::from(cadence.num);
        let den = i128::from(cadence.den).max(1);
        let ticks = (i128::from(secs).saturating_mul(num)) / den;
        Some(u64::try_from(ticks.max(0)).unwrap_or(u64::MAX))
    }
}
