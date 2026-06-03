//! The `mosaic` daemon/CLI entrypoint.
//!
//! A thin shell over the [`mosaic_cli`] library: it initializes structured
//! logging, parses the [`mosaic_cli::cli::Cli`] grammar, and dispatches to the
//! `validate` / `run` subcommands. The user-facing report text is printed to
//! stdout here — the only place the workspace `print_stdout` ban is relaxed,
//! because this is the human-facing terminal surface, not engine or data-plane
//! code.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    // reason: the `mosaic` binary prints human-facing reports to stdout and
    // fatal startup errors (when `tracing` is unavailable) to stderr. This is
    // the terminal surface, not engine or data-plane code.
)]

use std::path::Path;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser as _;
use mosaic_cli::cli::{Cli, Command, RunArgs, ValidateArgs};
use mosaic_cli::run::{HeadlessEngine, RunReport};
use mosaic_cli::validate::validate_config;
use mosaic_config::MosaicConfig;
use mosaic_engine::StopSignal;
use mosaic_telemetry::tracing_init::SubscriberBuilder;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(err) = init_tracing() {
        // Logging failed to initialize; report on stderr (tracing is unavailable)
        // and bail. This is startup code, not the data plane.
        eprintln!("mosaic: failed to initialize logging: {err:#}");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "command failed");
            eprintln!("mosaic: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Initialize the structured `tracing` subscriber (stderr, env-overridable via
/// `RUST_LOG`, defaulting to `info`).
fn init_tracing() -> anyhow::Result<()> {
    SubscriberBuilder::new()
        .with_default_level("info")
        .with_env(true)
        .try_init()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Dispatch a parsed [`Cli`] to its subcommand, returning the process exit code.
async fn dispatch(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Validate(args) => run_validate(&args),
        Command::Run(args) => run_run(args).await,
    }
}

/// The `validate` subcommand: validate one config and print its report. Exits
/// non-zero if the config is invalid.
fn run_validate(args: &ValidateArgs) -> anyhow::Result<ExitCode> {
    let report = validate_config(&args.config)?;
    println!("{}", report.render());
    Ok(if report.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The `run` subcommand: validate, build the engine, and (in `--headless` mode)
/// drive the software output clock; otherwise build + report readiness.
async fn run_run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let config = load_validated(&args.config)?;

    if !args.headless {
        // The GPU/FFmpeg-backed engine, the control plane (mosaic-control), and
        // the output servers are not yet runnable from this crate (the control
        // crate is a non-functional scaffold and the media backends are
        // off-by-default, not-yet-implemented features). Rather than fake a
        // running daemon, build the software engine, report readiness, and steer
        // the operator to `--headless` for the software end-to-end smoke.
        let engine = HeadlessEngine::build(&config)?;
        println!(
            "ready: built engine for {} source(s) at {}/{} fps; \
             a non-headless run (GPU/FFmpeg backends + control plane) is not yet wired — \
             use `--headless` for the software output-clock smoke.",
            engine.source_count(),
            engine.cadence().num,
            engine.cadence().den,
        );
        return Ok(ExitCode::SUCCESS);
    }

    let mut engine = HeadlessEngine::build(&config)?;
    let report = if let Some(ticks) = args.ticks {
        tracing::info!(ticks, "headless run: bounded");
        engine
            .run_for_realtime(ticks)
            .await
            .context("headless bounded run")?
    } else {
        tracing::info!("headless run: until Ctrl-C");
        run_until_ctrl_c(&mut engine).await?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Drive the headless engine until Ctrl-C, then return the run report.
///
/// A best-effort signal watcher raises the engine's stop flag on Ctrl-C; the
/// engine checks it once per tick and finishes the current frame cleanly. The
/// watcher cannot back-pressure the engine (invariant #10).
async fn run_until_ctrl_c(engine: &mut HeadlessEngine) -> anyhow::Result<RunReport> {
    let stop = StopSignal::new();
    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received; stopping after the current frame");
            stop_for_signal.stop();
        }
    });

    let report = engine
        .run_until_stopped(&stop)
        .await
        .context("headless run until Ctrl-C")?;
    signal.abort();
    Ok(report)
}

/// Load and validate a config, failing with a clear error if it is invalid.
fn load_validated(path: &Path) -> anyhow::Result<MosaicConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config = MosaicConfig::load_from_toml(&text)
        .with_context(|| format!("parsing config {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("validating config {}", path.display()))?;
    Ok(config)
}
