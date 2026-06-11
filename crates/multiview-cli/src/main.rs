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
use multiview_cli::run::{RunReport, SoftwareEngine};
use multiview_cli::validate::validate_config;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, EngineStateSnapshot};
#[cfg(feature = "ffmpeg")]
use multiview_engine::{ActorExit, Program, ProgramId, ProgramSet, RealtimePacer};
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;
use multiview_telemetry::tracing_init::SubscriberBuilder;

/// The boxed per-tick command drain the engine applies at the frame boundary
/// (the control-plane command bus → live reconfiguration), shared by the
/// software-engine and full-pipeline run paths.
///
/// `Send` so the full-pipeline run can be driven on a spawned supervised task
/// under the engine `ProgramSet` (MP-1, ADR-0030 §2.2). The drain runs on the
/// output-clock loop and must be non-blocking (invariants #1 + #10).
type ControlDrain = Box<dyn FnMut(&mut CompositorDrive<Nv12Image>) + Send>;

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

/// The `run` subcommand: validate the config, then either drive the FFmpeg-free
/// software engine (`--software`), the full libav\* pipeline (default, `ffmpeg`
/// feature), or — with neither available — report readiness.
async fn run_run(args: RunArgs) -> anyhow::Result<ExitCode> {
    // Review M3: canonicalize the boot path up front so a symlinked config is
    // watched, promoted, and state-dir-anchored at its REAL file — a promote's
    // atomic rename must replace the file, never the symlink pointing at it.
    // A canonicalization failure keeps the given path (the load below reports
    // any real problem with it).
    let mut args = args;
    if let Ok(canonical) = args.config.canonicalize() {
        args.config = canonical;
    }
    let boot = load_validated(&args.config)?;
    // Boot/Loaded/Running (ADR-W022 §4): resolve the starting Running state —
    // under `[control] start = "resume"` a valid persisted `active.toml`
    // becomes the starting document wholesale (the engine is built from it);
    // Loaded stays the boot snapshot and the boot file stays the watch target.
    let start = multiview_cli::boot::resolve_start_config(boot, &args.config);

    if args.software {
        return run_software(&start, &args).await;
    }

    run_pipeline(&start, &args).await
}

/// The FFmpeg-free software run: the output-clock + CPU compositor driving the
/// built-in test-pattern sources (the software end-to-end smoke of the
/// output-clock invariant), serving the API/WebUI just like the full build.
async fn run_software(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    let config = &start.running;
    let mut engine = SoftwareEngine::build(config)?;
    let cadence = engine.cadence();
    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "software run: bounded");
        engine
            .run_for_realtime(ticks)
            .await
            .context("software bounded run")?
    } else {
        tracing::info!("software run: until Ctrl-C");
        run_software_until_ctrl_c(&mut engine, start, &args.config).await?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The full libav\* end-to-end pipeline (the `ffmpeg` feature): ingest →
/// composite → encode-once → fan out to the configured file/HLS outputs.
#[cfg(feature = "ffmpeg")]
async fn run_pipeline(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    use multiview_cli::pipeline::Pipeline;

    let config = &start.running;
    let mut pipeline = Pipeline::build(config).context("building the pipeline")?;
    if let Some(subs) = &args.subtitles {
        let track = load_subtitles(subs)
            .with_context(|| format!("loading subtitles {}", subs.display()))?;
        tracing::info!(file = %subs.display(), cues = track.len(), "burning in subtitles");
        pipeline = pipeline.with_subtitles(track);
    }
    if args.program_audio {
        tracing::info!("program audio enabled: muxing an AAC program-audio stream");
        pipeline.enable_program_audio();
    }
    let cadence = pipeline.cadence();
    tracing::info!(
        sources = pipeline.source_count(),
        encoder = pipeline.encoder_name(),
        "pipeline built"
    );

    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "pipeline run: bounded");
        pipeline
            .run_for(ticks)
            .await
            .context("bounded pipeline run")?
    } else {
        tracing::info!("pipeline run: until Ctrl-C");
        // MP-1 (ADR-0030 §2.2): the daemon run path builds an engine `ProgramSet`
        // and drives this single program (id "main") through it — move the owned
        // pipeline in (the set spawns it on its own supervised task).
        run_pipeline_until_ctrl_c(pipeline, start, &args.config).await?
    };

    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// The debounce window for the ADR-W022 Running persister: at most one
/// `active.toml` write per window (control-plane file I/O only; inv #10).
const RUNNING_PERSIST_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(2);

/// The run's handle on the debounced ADR-W022 Running persister: the spawned
/// task plus the served [`multiview_control::AppState`] it persists from.
struct RunningPersist {
    /// The debounced persist task ([`multiview_control::boot_model::spawn_running_persist`]).
    task: tokio::task::JoinHandle<()>,
    /// The served control-plane state (one set of stores with the router).
    state: multiview_control::AppState,
}

impl RunningPersist {
    /// Stop the debounced persister at teardown and capture changes younger
    /// than the debounce with one final best-effort persist (fail-soft: a
    /// failure is warned and the shutdown continues).
    fn finish(self) {
        self.task.abort();
        if let Err(error) = multiview_control::boot_model::persist_running_now(&self.state) {
            tracing::warn!(
                error = %error,
                "the final running-state persist at shutdown was skipped (fail-soft)"
            );
        }
    }
}

/// Bring up the management control plane for a run (one wiring for BOTH run
/// paths — ADR-W013/ADR-W018): the live-source hub over the run's per-source
/// stop registry + the shared (live-updatable) preview store map, the preview
/// provider, the bounded command bus, the bound server, the ADR-W020
/// config-file watcher over `config_path` (external file edits hot-reload the
/// impacted parts through the same command bus; an invalid file changes
/// nothing), and the ADR-W022 Boot/Loaded/Running model (the Loaded snapshot
/// persisted to `loaded.toml`, the debounced `active.toml` Running persister,
/// and the boot-model/revert/promote API surface).
///
/// `start` is the run's resolved cold-start state; its `running` document is
/// what the engine was built from, so it seeds the stores AND becomes the
/// watcher's diff baseline (under a resume the baseline is the resumed
/// document while `config_path` stays the boot file — pin (b)).
///
/// Returns the server task handle, the engine-side [`multiview_control::CommandReceiver`]
/// (the caller builds its path-specific frame-boundary drain from it), the
/// [`multiview_cli::live_sources::LiveSourceHub`] (shut down after the run loop
/// returns), the config-watch handle (stop it at teardown; its
/// `expect_write` seam suppresses server-side writes), and the Running
/// persister (call [`RunningPersist::finish`] at teardown). The hub shares
/// `registry`, so a live remove can tear down a startup producer (generator or
/// ingest thread) too.
#[allow(clippy::too_many_arguments)]
// reason: this is the single private wiring helper both run paths call; its
// parameters (listen, start state, config path, publisher, preview slot,
// stores, stop registry, shutdown) are each distinct run-owned handles dictated
// by the control-plane surface. Bundling them into a struct would only move the
// arity without improving clarity for the two thin callers.
async fn serve_control_plane(
    listen: &str,
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    program_slot: multiview_cli::preview::ProgramSlot,
    stores: std::collections::HashMap<String, Arc<multiview_framestore::TileStore<Nv12Image>>>,
    registry: multiview_cli::live_sources::StopRegistry,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<(
    tokio::task::JoinHandle<std::io::Result<()>>,
    multiview_control::CommandReceiver,
    multiview_cli::live_sources::LiveSourceHub,
    multiview_cli::config_watch::ConfigWatchHandle,
    RunningPersist,
)> {
    let config = &start.running;
    let (commands, command_rx) = command_bus(64);
    // The live-source hub (ADR-W018): owns runtime producer spawn/teardown +
    // the SHARED, live-updatable preview store map, off the clock thread.
    let shared_stores = multiview_cli::live_sources::shared_stores(stores);
    let hub =
        multiview_cli::live_sources::LiveSourceHub::start(registry, Arc::clone(&shared_stores));
    // The live-preview provider reads the program slot the run loop fills + the
    // shared per-input store map — read-only for control (invariant #10).
    let provider: multiview_control::SharedPreview = Arc::new(
        multiview_cli::preview::CliPreviewProvider::new(program_slot, shared_stores),
    );
    // The Boot/Loaded/Running model (ADR-W022): Loaded is the immutable boot
    // snapshot; the model backs `GET /api/v1/config/boot-model` and the
    // revert-to-start/promote actions.
    let boot_model = Arc::new(start.to_boot_model(config_path));
    let (addr, handle, state) = control::bind_and_serve(
        listen,
        config,
        Arc::clone(publisher),
        commands,
        provider,
        Some(Arc::clone(&boot_model)),
        async move {
            let _ = shutdown_rx.await;
        },
    )
    .await
    .with_context(|| format!("binding the control plane on {listen}"))?;
    tracing::info!(listen = %addr, "control plane listening (OpenAPI/Scalar docs at /docs)");
    // Persist the Loaded snapshot to `loaded.toml` (forensics + the on-disk
    // revert target record). Fail-soft: the in-memory snapshot is
    // authoritative; a write failure is warned, never fatal.
    if let Err(reason) = multiview_control::boot_model::persist_loaded(&boot_model) {
        tracing::warn!(
            reason = %reason,
            "could not persist the Loaded snapshot (fail-soft; the in-memory snapshot is authoritative)"
        );
    }
    // The debounced Running persister (ADR-W022 §3): waits on the ONE
    // `running_changed` choke-point signal the audit recorder fires, then
    // writes `active.toml` atomically. Control-plane file I/O only (inv #10).
    let persist = RunningPersist {
        task: multiview_control::boot_model::spawn_running_persist(
            state.clone(),
            RUNNING_PERSIST_DEBOUNCE,
        ),
        state: state.clone(),
    };
    // Watch the boot config file for external edits (ADR-W020): a valid write
    // hot-reloads the impacted parts through the SAME router state + command
    // bus; an invalid write warns and changes nothing. A control-plane tokio
    // tenant — it can never pace or stall the engine (inv #1/#10). The diff
    // baseline is the RUNNING document (the resumed one under resume).
    let watch = multiview_cli::config_watch::spawn(
        config_path.to_path_buf(),
        config.clone(),
        state,
        multiview_cli::config_watch::WatchOptions::default(),
    );
    Ok((handle, command_rx, hub, watch, persist))
}

/// Drive the ingest/composite/encode pipeline until Ctrl-C while **also**
/// serving the control plane, the embedded web UI, and the live program/input
/// previews from the SAME run (when `[control]` is configured) — ingestion,
/// processing, output, and management are one integrated process. The control
/// plane shares the engine's outbound publisher (read-only) and the live-preview
/// slot, and submits to the non-blocking command bus the pipeline drains at each
/// frame boundary; none of it can back-pressure the output clock (inv #1 + #10).
#[cfg(feature = "ffmpeg")]
async fn run_pipeline_until_ctrl_c(
    pipeline: multiview_cli::pipeline::Pipeline,
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    let config = &start.running;
    let stop = StopSignal::new();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = multiview_cli::preview::program_slot();
    // This program's cadence (the legacy single program's canvas fps) for the
    // engine `ProgramSet` member metadata.
    let cadence = pipeline.cadence();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    #[allow(clippy::type_complexity)]
    // reason: the one-shot wiring tuple of this run path's control-plane
    // handles (server task, drain, hub, watch, persister), each `Option`al on
    // whether `[control]` is configured. A named struct would be used once.
    let (server, drain, live_hub, config_watch, running_persist): (
        Option<_>,
        ControlDrain,
        Option<_>,
        Option<_>,
        Option<RunningPersist>,
    ) = if let Some(cfg) = config.control.as_ref() {
        let (handle, command_rx, hub, watch, persist) = serve_control_plane(
            &cfg.listen,
            start,
            config_path,
            &publisher,
            Arc::clone(&preview_slot),
            pipeline.preview_stores(),
            pipeline.stop_registry(),
            shutdown_rx,
        )
        .await?;
        // Thread the run's live subtitle re-point seam (RT-10b) into the drain so a
        // `RouteSubtitle` (RT-11) reaches the running pipeline's layer. The slot is
        // shared (lock-free `ArcSwapOption`); the run publishes its handle into it at
        // drive start, and the drain reads it wait-free (inv #1/#10). Only under
        // `overlay` (without it the run renders no subtitles, so there is no layer).
        // The live-source seam (ADR-W018) rides both variants.
        #[cfg(feature = "overlay")]
        let drain: ControlDrain = Box::new(control::command_drain_with_seams(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            pipeline.subtitle_route_slot(),
            hub.handle(),
        ));
        #[cfg(not(feature = "overlay"))]
        let drain: ControlDrain = Box::new(control::command_drain_with_live_sources(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            hub.handle(),
        ));
        (Some(handle), drain, Some(hub), Some(watch), Some(persist))
    } else {
        drop(shutdown_rx);
        (
            None,
            Box::new(|_d: &mut CompositorDrive<Nv12Image>| {}),
            None,
            None,
            None,
        )
    };

    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received; stopping after the current frame");
            stop_for_signal.stop();
        }
    });

    // Sample CPU/host-memory/per-GPU load at ~1.3 Hz and PUSH `Event::SystemMetrics`
    // onto the SAME outbound publisher the control plane forwards to the WebUI
    // footer (the full-pipeline path serves the control plane, so the poller must
    // live here too — not only in the software-only run). The publish never
    // awaits/blocks a slow subscriber (inv #10); the task self-stops on `stop`.
    let metrics_task = multiview_cli::system_metrics::spawn(
        Arc::clone(&publisher),
        multiview_cli::system_metrics::default_load_source(),
        stop.clone(),
        None,
    );

    // SA-0 (ADR-0035): at build time, off the output-clock thread (the clock is
    // not yet constructed → inv #1), cross-check the wgpu compositor adapter
    // against discovered hardware. If a real GPU is present but compositing
    // resolved a software/CPU adapter (the silent fallback), emit a latched,
    // actionable `gpu-present-no-vulkan-adapter` warning through the SAME
    // drop-oldest publisher (inv #10) so the operator sees a banner + a
    // `GET /api/v1/health` entry instead of a silent CPU burn. Only under `gpu`
    // (without it the CPU composite is the intentional choice → nothing emitted).
    #[cfg(feature = "gpu")]
    {
        let since_nanos = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        )
        .unwrap_or(i64::MAX);
        let _ = multiview_cli::capability_warn::probe_and_emit(
            publisher.as_ref(),
            multiview_cli::system_metrics::default_load_source().as_ref(),
            since_nanos,
        );
    }

    // MP-1 (ADR-0030 §2.2): build the engine `ProgramSet` and drive this single
    // program (id "main") through it — behaviour-identical to today (one program,
    // the same drive/stop/publisher/preview/drain). See `drive_main_program_in_set`.
    let report =
        drive_main_program_in_set(pipeline, cadence, &stop, &publisher, &preview_slot, drain)
            .await?;

    // The pipeline loop returned; stop the metrics poller (it also self-stops on
    // the StopSignal within one sample period), the config-file watcher, the
    // Running persister (with one final best-effort `active.toml` capture —
    // ADR-W022 §3), and tear down the live-source hub (it stops + joins every
    // runtime producer).
    metrics_task.abort();
    if let Some(watch) = config_watch {
        watch.stop();
    }
    if let Some(persist) = running_persist {
        persist.finish();
    }
    if let Some(hub) = live_hub {
        hub.shutdown();
    }

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

/// Drive the single legacy `"main"` program through an engine
/// [`ProgramSet`](multiview_engine::ProgramSet) (MP-1, ADR-0030 §2.2).
///
/// For the legacy single-program config the set has **exactly one** program (id
/// `"main"`) — behaviour-identical to driving the [`Pipeline`] directly: the same
/// `run_until_serving` drive, the same `StopSignal` (Ctrl-C reaches the program via
/// the supervisor's per-program stop handle), the same publisher/preview/drain. The
/// set owns the program's lifecycle (spawn on its own supervised task, stop, join)
/// and samples its **live** `ticks_emitted` off a shared counter the pipeline
/// increments per tick — exactly the N-concurrent-programs machinery, exercised
/// here at N=1. MP-5 routes the config's `[[programs]]` into the same
/// `ProgramSet::start` for N>1.
///
/// # Errors
///
/// Propagates a failure to admit/start the program, or the run's own error.
#[cfg(feature = "ffmpeg")]
async fn drive_main_program_in_set(
    pipeline: multiview_cli::pipeline::Pipeline,
    cadence: multiview_core::time::Rational,
    stop: &StopSignal,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    preview_slot: &multiview_cli::preview::ProgramSlot,
    drain: ControlDrain,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    // The shared monotonic reference every program in the set reads (its one program
    // reads it for its own clock's seed; identical to the inline `Monotonic` source
    // the pipeline built before).
    let mut programs: ProgramSet<RealtimePacer> =
        ProgramSet::new(Arc::new(multiview_engine::MonotonicTimeSource::new()));
    let program_id = ProgramId::new(ProgramId::MAIN).context("the reserved \"main\" program id")?;
    // The live per-tick counter the `ProgramSet` samples for "main": the pipeline
    // increments it once per emitted output tick (a single wait-free `fetch_add`),
    // so `programs.ticks_emitted("main")` is genuinely live, not fabricated.
    let main_ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Recover the run's `PipelineReport` from the supervised task.
    let (report_tx, report_rx) =
        tokio::sync::oneshot::channel::<Result<multiview_cli::pipeline::PipelineReport, String>>();

    let run_stop = stop.clone();
    let run_publisher = Arc::clone(publisher);
    let run_preview = Arc::clone(preview_slot);
    let run_ticks = Arc::clone(&main_ticks);
    let program = Program::<RealtimePacer>::from_runner(
        program_id,
        cadence,
        run_stop.clone(),
        main_ticks,
        move || {
            Box::pin(async move {
                let mut pipeline = pipeline;
                let outcome = pipeline
                    .run_until_serving_observed(
                        &run_stop,
                        run_publisher.as_ref(),
                        &run_preview,
                        drain,
                        Some(run_ticks),
                    )
                    .await;
                let exit = if outcome.is_ok() {
                    ActorExit::Completed
                } else {
                    ActorExit::Failed
                };
                let _ = report_tx.send(outcome.map_err(|e| e.to_string()));
                exit
            })
        },
    );
    programs
        .start(program)
        .context("starting the \"main\" program in the ProgramSet")?;

    // Await the single program's NATURAL completion: its run returns when the
    // StopSignal is raised (Ctrl-C), at which point it sends its report. We await
    // that first, THEN `shutdown` (raise any still-running program's stop — a no-op
    // here — and join every supervised task so no task is left detached).
    let recovered = report_rx.await;
    programs.shutdown().await;
    match recovered {
        Ok(Ok(report)) => Ok(report),
        Ok(Err(reason)) => Err(anyhow::anyhow!(reason)).context("pipeline run until Ctrl-C"),
        Err(_) => Err(anyhow::anyhow!(
            "the \"main\" program task ended without reporting"
        )),
    }
}

/// Without the `ffmpeg` feature this build has no libav decoders, so external
/// ingest/encode is unavailable: build the software engine, report readiness,
/// and steer the operator to `--software` (or a build with `--features ffmpeg`)
/// rather than pretending a daemon is ingesting sources it cannot decode.
#[cfg(not(feature = "ffmpeg"))]
#[allow(clippy::unused_async)]
// reason: this is the no-`ffmpeg` half of an `async fn` pair; the `ffmpeg`
// counterpart awaits the full pipeline, so the signature must match for the one
// `run_pipeline(..).await` call site to compile under either feature set.
async fn run_pipeline(
    start: &multiview_cli::boot::StartConfig,
    args: &RunArgs,
) -> anyhow::Result<ExitCode> {
    let config = &start.running;
    if args.subtitles.is_some() {
        tracing::warn!(
            "--subtitles needs the `ffmpeg`+`overlay` features (full pipeline); ignoring"
        );
    }
    if args.program_audio {
        tracing::warn!("--program-audio needs the `ffmpeg` feature (full pipeline); ignoring");
    }
    let engine = SoftwareEngine::build(config)?;
    println!(
        "ready: built engine for {} source(s) at {}/{} fps; \
         this build has no `ffmpeg` feature, so an external ingest/encode run is unavailable — \
         use `--software` for the output-clock smoke, or rebuild with \
         `--features ffmpeg` (add `gpl-codecs` for software H.264/H.265).",
        engine.source_count(),
        engine.cadence().num,
        engine.cadence().den,
    );
    Ok(ExitCode::SUCCESS)
}

/// Drive the software engine until Ctrl-C, then return the run report.
///
/// A best-effort signal watcher raises the engine's stop flag on Ctrl-C; the
/// engine checks it once per tick and finishes the current frame cleanly. The
/// watcher cannot back-pressure the engine (invariant #10).
async fn run_software_until_ctrl_c(
    engine: &mut SoftwareEngine,
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
) -> anyhow::Result<RunReport> {
    let config = &start.running;
    let stop = StopSignal::new();

    // The engine's outbound publisher, shared read-only with the control plane
    // when it is enabled: the API/WebUI observe live engine state through the
    // wait-free latest-state slot + drop-oldest event broadcast, never able to
    // back-pressure the engine (invariant #10). 64 = the broadcast ring depth.
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));

    // Bring up the control server iff `[control]` is configured, and build the
    // per-tick command drain the engine applies at the frame boundary. The drain
    // is boxed so the run call is uniform: the live command-bus drain when the
    // control plane is up, a no-op otherwise. The server serves until
    // `shutdown_rx` resolves (once the engine loop returns).
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    #[allow(clippy::type_complexity)]
    // reason: the one-shot wiring tuple of this run path's control-plane
    // handles (server task, drain, hub, watch, persister), each `Option`al on
    // whether `[control]` is configured. A named struct would be used once.
    let (server, drain, live_hub, config_watch, running_persist): (
        Option<_>,
        ControlDrain,
        Option<_>,
        Option<_>,
        Option<RunningPersist>,
    ) = if let Some(cfg) = config.control.as_ref() {
        let (handle, command_rx, hub, watch, persist) = serve_control_plane(
            &cfg.listen,
            start,
            config_path,
            &publisher,
            engine.program_preview(),
            engine.preview_stores(),
            engine.stop_registry(),
            shutdown_rx,
        )
        .await?;
        let drain: ControlDrain = Box::new(control::command_drain_with_live_sources(
            command_rx,
            config.clone(),
            Arc::clone(&publisher),
            hub.handle(),
        ));
        (Some(handle), drain, Some(hub), Some(watch), Some(persist))
    } else {
        drop(shutdown_rx);
        (
            None,
            Box::new(|_d: &mut CompositorDrive<Nv12Image>| {}),
            None,
            None,
            None,
        )
    };

    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("Ctrl-C received; stopping after the current frame");
            stop_for_signal.stop();
        }
    });

    // The off-hot-path system-metrics poller: samples whole-system CPU + host
    // memory + per-GPU load at ~1.3 Hz and PUSHES `Event::SystemMetrics` onto the
    // engine's outbound event stream (the same drop-oldest broadcast the control
    // plane forwards to the WebUI footer). The publish never awaits/blocks a slow
    // subscriber (invariant #10), and the task self-stops on the run's StopSignal.
    // `program_fps` is left `None`: the software run exposes no live measured-fps
    // counter to this task, and the configured cadence is a target — not a
    // measured rate — so we do not fabricate one.
    let metrics_task = multiview_cli::system_metrics::spawn(
        Arc::clone(&publisher),
        multiview_cli::system_metrics::default_load_source(),
        stop.clone(),
        None,
    );

    let report = engine
        .run_until_stopped_with_control(&stop, publisher.as_ref(), drain)
        .await
        .context("headless run until Ctrl-C")?;

    // The engine loop returned; stop the metrics poller (it also self-stops on the
    // StopSignal within one sample period), the config-file watcher, the Running
    // persister (with one final best-effort `active.toml` capture — ADR-W022 §3),
    // tear down the live-source hub (it stops + joins every runtime producer),
    // and bring the control server down.
    metrics_task.abort();
    if let Some(watch) = config_watch {
        watch.stop();
    }
    if let Some(persist) = running_persist {
        persist.finish();
    }
    if let Some(hub) = live_hub {
        hub.shutdown();
    }
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
