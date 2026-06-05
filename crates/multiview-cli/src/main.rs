//! The `multiview` daemon/CLI entrypoint.
//!
//! A thin shell over the [`multiview_cli`] library: it initializes structured
//! logging, parses the [`multiview_cli::cli::Cli`] grammar, and dispatches to the
//! `validate` / `run` subcommands. The user-facing report text is printed to
//! stdout here — the only place the workspace `print_stdout` ban is relaxed,
//! because this is the human-facing terminal surface, not engine or data-plane
//! code.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    // reason: the `multiview` binary prints human-facing reports to stdout and
    // fatal startup errors (when `tracing` is unavailable) to stderr. This is
    // the terminal surface, not engine or data-plane code.
)]

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser as _;
use multiview_cli::cli::{Cli, Command, RunArgs, ValidateArgs};
use multiview_cli::control;
use multiview_cli::run::{HeadlessEngine, RunReport};
use multiview_cli::validate::validate_config;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, EngineStateSnapshot};
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;
use multiview_telemetry::tracing_init::SubscriberBuilder;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(err) = init_tracing() {
        // Logging failed to initialize; report on stderr (tracing is unavailable)
        // and bail. This is startup code, not the data plane.
        eprintln!("multiview: failed to initialize logging: {err:#}");
        return ExitCode::FAILURE;
    }

    let cli = Cli::parse();
    match dispatch(cli).await {
        Ok(code) => code,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "command failed");
            eprintln!("multiview: error: {err:#}");
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

/// The `run` subcommand: validate the config, then either drive the pure-
/// software headless engine (`--headless`), the real libav\* pipeline (default,
/// `ffmpeg` feature), or — with neither available — report readiness.
async fn run_run(args: RunArgs) -> anyhow::Result<ExitCode> {
    let config = load_validated(&args.config)?;

    if args.headless {
        return run_headless(&config, &args).await;
    }

    run_real(&config, &args).await
}

/// The pure-software, FFmpeg-free headless run (software end-to-end smoke of the
/// output-clock invariant).
async fn run_headless(config: &MultiviewConfig, args: &RunArgs) -> anyhow::Result<ExitCode> {
    let mut engine = HeadlessEngine::build(config)?;
    let cadence = engine.cadence();
    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "headless run: bounded");
        engine
            .run_for_realtime(ticks)
            .await
            .context("headless bounded run")?
    } else {
        tracing::info!("headless run: until Ctrl-C");
        run_until_ctrl_c(&mut engine, config).await?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The real libav\* end-to-end pipeline (the `ffmpeg` feature): ingest →
/// composite → encode-once → fan out to the configured file/HLS outputs.
#[cfg(feature = "ffmpeg")]
async fn run_real(config: &MultiviewConfig, args: &RunArgs) -> anyhow::Result<ExitCode> {
    use multiview_cli::pipeline::RealPipeline;

    let mut pipeline = RealPipeline::build(config).context("building the real pipeline")?;
    if let Some(subs) = &args.subtitles {
        let track = load_subtitles(subs)
            .with_context(|| format!("loading subtitles {}", subs.display()))?;
        tracing::info!(file = %subs.display(), cues = track.len(), "burning in subtitles");
        pipeline = pipeline.with_subtitles(track);
    }
    let cadence = pipeline.cadence();
    tracing::info!(
        sources = pipeline.source_count(),
        encoder = pipeline.encoder_name(),
        "real pipeline built"
    );

    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "real run: bounded");
        pipeline.run_for(ticks).await.context("real bounded run")?
    } else {
        tracing::info!("real run: until Ctrl-C");
        let stop = StopSignal::new();
        let stop_for_signal = stop.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl-C received; stopping after the current frame");
                stop_for_signal.stop();
            }
        });
        let report = pipeline
            .run_until(&stop)
            .await
            .context("real run until Ctrl-C")?;
        signal.abort();
        report
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Without the `ffmpeg` feature there is no real ingest/encode path: build the
/// software engine, report readiness, and steer the operator to `--headless`
/// (or a build with `--features ffmpeg`) rather than faking a running daemon.
#[cfg(not(feature = "ffmpeg"))]
#[allow(clippy::unused_async)]
// reason: this is the no-`ffmpeg` half of an `async fn` pair; the `ffmpeg`
// counterpart awaits the real pipeline, so the signature must match for the one
// `run_real(..).await` call site to compile under either feature set.
async fn run_real(config: &MultiviewConfig, args: &RunArgs) -> anyhow::Result<ExitCode> {
    if args.subtitles.is_some() {
        tracing::warn!(
            "--subtitles needs the `ffmpeg`+`overlay` features (real pipeline); ignoring"
        );
    }
    let engine = HeadlessEngine::build(config)?;
    println!(
        "ready: built engine for {} source(s) at {}/{} fps; \
         this build has no `ffmpeg` feature, so a real ingest/encode run is unavailable — \
         use `--headless` for the software output-clock smoke, or rebuild with \
         `--features ffmpeg` (add `gpl-codecs` for software H.264/H.265).",
        engine.source_count(),
        engine.cadence().num,
        engine.cadence().den,
    );
    Ok(ExitCode::SUCCESS)
}

/// Drive the headless engine until Ctrl-C, then return the run report.
///
/// A best-effort signal watcher raises the engine's stop flag on Ctrl-C; the
/// engine checks it once per tick and finishes the current frame cleanly. The
/// watcher cannot back-pressure the engine (invariant #10).
async fn run_until_ctrl_c(
    engine: &mut HeadlessEngine,
    config: &MultiviewConfig,
) -> anyhow::Result<RunReport> {
    /// The boxed per-tick command drain the engine applies at the frame boundary.
    type Drain = Box<dyn FnMut(&mut CompositorDrive<Nv12Image>)>;

    let stop = StopSignal::new();

    // The engine's outbound publisher, shared read-only with the control plane
    // when it is enabled: the API/WebUI observe live engine state through the
    // wait-free latest-state slot + drop-oldest event broadcast, never able to
    // back-pressure the engine (invariant #10). 64 = the broadcast ring depth.
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));

    // Bring up the control server iff `[control]` is configured, and build the
    // per-tick command drain the engine applies at the frame boundary. The drain
    // is boxed so the run call is uniform: the real command-bus drain when the
    // control plane is up, a no-op otherwise. The server serves until
    // `shutdown_rx` resolves (once the engine loop returns).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (server, drain): (Option<_>, Drain) = if let Some(cfg) = config.control.as_ref() {
        let (commands, command_rx) = command_bus(64);
        let (addr, handle) =
            control::bind_and_serve(&cfg.listen, Arc::clone(&publisher), commands, async move {
                let _ = shutdown_rx.await;
            })
            .await
            .with_context(|| format!("binding the control plane on {}", cfg.listen))?;
        tracing::info!(listen = %addr, "control plane listening (OpenAPI/Scalar docs at /docs)");
        (
            Some(handle),
            Box::new(control::command_drain(command_rx, config.clone())),
        )
    } else {
        drop(shutdown_rx);
        (None, Box::new(|_d: &mut CompositorDrive<Nv12Image>| {}))
    };

    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received; stopping after the current frame");
            stop_for_signal.stop();
        }
    });

    let report = engine
        .run_until_stopped_with_control(&stop, publisher.as_ref(), drain)
        .await
        .context("headless run until Ctrl-C")?;

    // The engine loop returned; bring the control server down gracefully.
    let _ = shutdown_tx.send(());
    if let Some(handle) = server {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "control server I/O error at shutdown"),
            Err(e) => tracing::warn!(error = %e, "control server task join error"),
        }
    }
    signal.abort();
    Ok(report)
}

/// Parse an external SRT/`WebVTT` subtitle file into a [`CueTrack`]. The format
/// is chosen by the extension (`.vtt`/`.webvtt` ⇒ `WebVTT`, otherwise `SubRip`).
#[cfg(feature = "ffmpeg")]
fn load_subtitles(path: &Path) -> anyhow::Result<multiview_overlay::subtitle::CueTrack> {
    use multiview_overlay::subtitle::CueTrack;
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading subtitles {}", path.display()))?;
    let is_vtt = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("vtt") || e.eq_ignore_ascii_case("webvtt"));
    let track = if is_vtt {
        CueTrack::parse_vtt(&text)
    } else {
        CueTrack::parse_srt(&text)
    }
    .with_context(|| format!("parsing subtitles {}", path.display()))?;
    Ok(track)
}

/// Load and validate a config, failing with a clear error if it is invalid.
fn load_validated(path: &Path) -> anyhow::Result<MultiviewConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config = MultiviewConfig::load_from_toml(&text)
        .with_context(|| format!("parsing config {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("validating config {}", path.display()))?;
    Ok(config)
}
