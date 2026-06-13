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
use multiview_cli::cli::{Cli, Command, NodeArgs, RunArgs, ValidateArgs};
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
        Command::Node(args) => run_node(args).await,
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
    let (boot_text, boot) = load_validated(&args.config)?;
    // Boot/Loaded/Running (ADR-W022 §4): resolve the starting Running state —
    // under `[control] start = "resume"` a valid persisted `active.toml`
    // becomes the starting document (with the boot file's restart-only
    // sections spliced in — review M1); Loaded stays the boot snapshot and
    // the boot file stays the watch target.
    let start = multiview_cli::boot::resolve_start_config(boot, boot_text, &args.config);

    // A configured `[timing].ptp_phc` in a build without the `ptp` feature is
    // a capability this binary cannot provide: fail the run at startup with a
    // clear error (the DEV-B1 display-output fail-fast precedent) — never
    // silently ride the system clock while the config asks for a PHC.
    multiview_cli::timing_gate::ensure_ptp_phc_supported(start.running.timing.as_ref())
        .map_err(|reason| anyhow::anyhow!(reason))?;

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

/// Raise `stop` when the process receives Ctrl-C (SIGINT) or — on Unix —
/// SIGTERM (review m2: `docker stop` and systemd send SIGTERM, and the
/// graceful teardown — the final `active.toml` persist included — must run
/// for both). The watcher task cannot back-pressure the engine (inv #10);
/// the run loop observes `stop` once per tick and finishes the current frame
/// cleanly.
fn spawn_stop_on_signal(stop: StopSignal) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut term) => {
                    tokio::select! {
                        result = tokio::signal::ctrl_c() => {
                            if result.is_ok() {
                                tracing::info!("Ctrl-C received; stopping after the current frame");
                                stop.stop();
                            }
                        }
                        _ = term.recv() => {
                            tracing::info!("SIGTERM received; stopping after the current frame");
                            stop.stop();
                        }
                    }
                }
                Err(error) => {
                    // SIGTERM registration failing is exotic (resource limits);
                    // degrade to Ctrl-C-only rather than dying silent.
                    tracing::warn!(
                        error = %error,
                        "SIGTERM handler unavailable; stopping on Ctrl-C only"
                    );
                    if tokio::signal::ctrl_c().await.is_ok() {
                        tracing::info!("Ctrl-C received; stopping after the current frame");
                        stop.stop();
                    }
                }
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("Ctrl-C received; stopping after the current frame");
                stop.stop();
            }
        }
    })
}

/// The `node` subcommand (ADR-0045 / DEV-B5): gate on the build features,
/// load → validate → lower the node document, then drive the **standard full
/// pipeline** over the lowered config — one supervised ingest (the unchanged
/// `multiview-input` pacer/jitter/normalize/reconnect stack) → the framestore
/// tile ladder (last-good, then the configured local slate) → single-source
/// full-canvas composite → the DEV-B1..B4 display sink(s) + ALSA HDMI audio.
/// `--ticks`/`--duration` bound the run (diagnostics/soak); otherwise the
/// node runs as the daemon until Ctrl-C/SIGTERM.
///
/// Presentation runs the DEV-C2 pull-side discipline: each head presents the
/// frame whose `wall_at(pts) + link_offset` is nearest the predicted next
/// vblank (repeat-if-early, drop-if-late), consuming the node document's
/// `timing.link_offset_ms` against the run's local outbound presentation epoch
/// (a future controller WS-client fills the same seam); a lost feed free-runs
/// on the held epoch so the heads never falter (inv #1/#10).
#[cfg(all(feature = "ffmpeg", feature = "display-kms"))]
async fn run_node(args: NodeArgs) -> anyhow::Result<ExitCode> {
    use multiview_cli::pipeline::Pipeline;

    multiview_cli::node::ensure_node_supported().map_err(|reason| anyhow::anyhow!(reason))?;
    let (node_cfg, config) = multiview_cli::node::load_node_run_config(&args.config)?;
    let mut pipeline = Pipeline::build(&config).context("building the node pipeline")?;
    // The rootless-container hotplug fallback cadence (kernel uevents stay
    // the primary path; ADR-0045 / display-out §10).
    pipeline.set_display_hotplug_poll(std::time::Duration::from_secs(node_cfg.hotplug.poll_secs));
    // DEV-C2: enable pull-side presentation discipline — the display heads
    // present the frame whose `wall_at(pts) + link_offset` is nearest the
    // predicted next vblank (repeat-if-early, drop-if-late), consuming the
    // node's `timing.link_offset_ms`. The epoch is the run's local outbound
    // presentation epoch (a future controller WS-client fills the same seam,
    // DEV-B6); a lost feed free-runs on the last epoch (inv #1/#10).
    let link_offset_ns =
        multiview_output::display::present::link_offset_ms_to_ns(node_cfg.timing.link_offset_ms);
    pipeline.set_node_presentation(link_offset_ns);
    let cadence = pipeline.cadence();
    tracing::info!(
        ingest = %node_cfg.ingest.url(),
        heads = node_cfg.displays.len(),
        link_offset_ms = node_cfg.timing.link_offset_ms,
        "node: pipeline built (DEV-C2 pull-side presentation discipline active: \
         wall_at(pts) + link_offset nearest the predicted vblank, free-running \
         on the last epoch if the feed drops)"
    );

    // The systemd integration (ADR-0045 deployment): best-effort sd_notify —
    // inert without NOTIFY_SOCKET (containers, dev shells).
    let notifier = multiview_cli::sdnotify::Notifier::from_env();

    let report = if let Some(ticks) = args.tick_budget(cadence) {
        tracing::info!(ticks, "node run: bounded");
        // A bounded run is a diagnostic/soak: READY up front (the daemon path
        // gates READY on the first frame boundary instead), STOPPING on the
        // way out even when the run errors.
        notifier.notify(&[
            multiview_cli::sdnotify::NotifyState::Ready,
            multiview_cli::sdnotify::NotifyState::Status("bounded node run"),
        ]);
        let outcome = pipeline.run_for(ticks).await.context("bounded node run");
        notifier.notify(&[multiview_cli::sdnotify::NotifyState::Stopping]);
        outcome?
    } else {
        tracing::info!("node run: until Ctrl-C/SIGTERM");
        run_node_until_signalled(pipeline, cadence, notifier).await?
    };
    println!("{}", report.render());
    Ok(if report.faltered {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Without `display-kms` + `ffmpeg` this build cannot run a display node:
/// report the gate's clear, actionable error (the DEV-B1 precedent — never a
/// silent skip).
#[cfg(not(all(feature = "ffmpeg", feature = "display-kms")))]
#[allow(clippy::unused_async)]
// reason: this is the unsupported half of an `async fn` pair; the supported
// counterpart awaits the full pipeline, so the signature must match for the
// one `run_node(..).await` call site to compile under either feature set.
async fn run_node(_args: NodeArgs) -> anyhow::Result<ExitCode> {
    match multiview_cli::node::ensure_node_supported() {
        Err(reason) => Err(anyhow::anyhow!(reason)),
        Ok(()) => Err(anyhow::anyhow!(
            "internal: the node support gate passed in a build without display-kms + ffmpeg"
        )),
    }
}

/// Drive the node pipeline until Ctrl-C (SIGINT) or SIGTERM (the systemd stop
/// signal), with **no control plane**: node enrollment/management is DEV-B6,
/// and a node must not silently open a listener nobody configured. The
/// outbound publisher exists because the drive path publishes engine state
/// through it (wait-free, invariant #10); nothing subscribes on a node.
#[cfg(all(feature = "ffmpeg", feature = "display-kms"))]
async fn run_node_until_signalled(
    pipeline: multiview_cli::pipeline::Pipeline,
    cadence: multiview_core::time::Rational,
    notifier: multiview_cli::sdnotify::Notifier,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    let stop = StopSignal::new();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = multiview_cli::preview::program_slot();

    let stop_for_signal = stop.clone();
    let signal = tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::warn!(error = %e, "SIGTERM handler unavailable; Ctrl-C only");
                    if ctrl_c.await.is_ok() {
                        tracing::info!("Ctrl-C received; stopping after the current frame");
                        stop_for_signal.stop();
                    }
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => tracing::info!("Ctrl-C received; stopping after the current frame"),
            _ = term.recv() => tracing::info!("SIGTERM received; stopping after the current frame"),
        }
        stop_for_signal.stop();
    });

    // The live per-tick counter (shared with the engine ProgramSet supervisor;
    // the DEV-B5 sd_notify watchdog samples the same counter so liveness pings
    // reflect the output clock actually advancing — invariant #1's signal).
    let ticks = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // READY=1 at the FIRST frame boundary: the drain runs on the output-clock
    // loop once per tick, so its first invocation means the display heads are
    // lit (the modeset precedes the loop) and the clock is emitting. The
    // notify is one non-blocking datagram syscall, sent exactly once
    // (invariants #1 + #10 hold).
    let ready_notifier = notifier.clone();
    let mut ready_sent = false;
    let drain: ControlDrain = Box::new(move |_d: &mut CompositorDrive<Nv12Image>| {
        if !ready_sent {
            ready_sent = true;
            ready_notifier.notify(&[
                multiview_cli::sdnotify::NotifyState::Ready,
                multiview_cli::sdnotify::NotifyState::Status(
                    "node presenting: output clock running, display head(s) lit",
                ),
            ]);
            tracing::info!("node: first frame boundary reached (sd_notify READY)");
        }
    });

    // The tick-gated watchdog (DEV-B5): pings WATCHDOG=1 at half the
    // WatchdogSec budget while the output clock advances; a stalled clock
    // withholds the ping so systemd restarts the node (invariant #1's
    // enforcement). No WATCHDOG_USEC (or an inert notifier) ⇒ no thread.
    let watchdog_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watchdog = if notifier.is_active() {
        multiview_cli::sdnotify::watchdog_interval_from_env().and_then(|interval| {
            spawn_node_watchdog(
                notifier.clone(),
                interval,
                Arc::clone(&ticks),
                Arc::clone(&watchdog_stop),
            )
        })
    } else {
        None
    };

    let report = drive_main_program_in_set(
        pipeline,
        cadence,
        &stop,
        &publisher,
        &preview_slot,
        drain,
        ticks,
    )
    .await;

    watchdog_stop.store(true, std::sync::atomic::Ordering::Release);
    if let Some(handle) = watchdog {
        if handle.join().is_err() {
            tracing::warn!("the node watchdog thread panicked during the run");
        }
    }
    notifier.notify(&[multiview_cli::sdnotify::NotifyState::Stopping]);
    signal.abort();
    report
}

/// Spawn the node's `sd_notify` watchdog thread: every `interval` (half the
/// systemd `WatchdogSec` budget) it samples the live output-tick counter and
/// sends `WATCHDOG=1` **only when the counter advanced** since the previous
/// check ([`multiview_cli::sdnotify::WatchdogGate`]). Returns [`None`] when
/// the thread cannot be spawned (logged; the node still runs — the watchdog
/// is an enforcement aid, not a dependency).
#[cfg(all(feature = "ffmpeg", feature = "display-kms"))]
fn spawn_node_watchdog(
    notifier: multiview_cli::sdnotify::Notifier,
    interval: std::time::Duration,
    ticks: Arc<std::sync::atomic::AtomicU64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    use std::sync::atomic::Ordering;
    let spawned = std::thread::Builder::new()
        .name("node-sd-watchdog".to_owned())
        .spawn(move || {
            let mut gate = multiview_cli::sdnotify::WatchdogGate::new();
            // Sleep in short slices so a stop request is honoured promptly
            // even under a multi-second ping interval.
            let slice = std::time::Duration::from_millis(250).min(interval);
            let mut next = std::time::Instant::now() + interval;
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(slice);
                if std::time::Instant::now() < next {
                    continue;
                }
                next += interval;
                if gate.should_ping(ticks.load(Ordering::Relaxed)) {
                    notifier.notify(&[multiview_cli::sdnotify::NotifyState::Watchdog]);
                } else {
                    tracing::warn!(
                        "output clock has not advanced this watchdog interval: withholding \
                         the systemd WATCHDOG ping (systemd will restart the node)"
                    );
                }
            }
        });
    match spawned {
        Ok(handle) => {
            tracing::info!(
                interval_ms = u64::try_from(interval.as_millis()).unwrap_or(u64::MAX),
                "systemd watchdog active (tick-gated pings at half WatchdogSec)"
            );
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not spawn the sd_notify watchdog thread");
            None
        }
    }
}

/// The run's handle on the debounced ADR-W022 Running persister: the spawned
/// task plus the served [`multiview_control::AppState`] it persists from.
struct RunningPersist {
    /// The debounced persist task ([`multiview_control::boot_model::spawn_running_persist`]).
    task: tokio::task::JoinHandle<()>,
    /// The served control-plane state (one set of stores with the router).
    state: multiview_control::AppState,
}

impl RunningPersist {
    /// Stop the debounced persister at teardown: abort → **await the task's
    /// termination** → one final best-effort persist capturing changes
    /// younger than the debounce (review M2: the ordering restores the
    /// deterministic `.tmp` single-writer guarantee; fail-soft on error).
    async fn finish(self) {
        multiview_control::boot_model::finish_running_persist(self.task, &self.state).await;
    }
}

/// The per-run-path inputs the control plane is wired from: the live preview
/// taps (the program slot the run loop fills + the per-source store map), the
/// producer stop registry the live-source hub shares with the run's startup
/// supervisors, the optional decoded-ingest spawner (ADR-W018 level 2 — the
/// full-pipeline path only), and what this run path can take live (ADR-W021 —
/// the binary is the only place that knows both the compiled features and the
/// path).
struct ControlPlaneWiring {
    /// The shared program-frame slot the run loop fills for previews.
    program_slot: multiview_cli::preview::ProgramSlot,
    /// The per-source last-good stores (the preview provider's initial map).
    stores: std::collections::HashMap<String, Arc<multiview_framestore::TileStore<Nv12Image>>>,
    /// The per-source producer stop registry (shared with the live-source hub).
    registry: multiview_cli::live_sources::StopRegistry,
    /// The decoded-ingest spawner (`Some` ⇔ network kinds live-apply, ADR-W018).
    ingest: Option<Arc<dyn multiview_cli::live_sources::IngestSpawner>>,
    /// What the running engine can take live (per-collection header honesty).
    live_apply: multiview_control::LiveApplyCaps,
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
/// persister (call [`RunningPersist::finish`] at teardown). The hub shares the
/// wiring's stop registry, so a live remove can tear down a startup producer
/// (generator or ingest thread) too.
///
/// `wiring.ingest` is the run's decoded-ingest spawner (ADR-W018 level 2): the
/// full-pipeline path passes `Pipeline::live_ingest_spawner` so network/file
/// sources spawn the same supervised `ingest_loop` live; the software path
/// passes `None`. The capability declared to the control plane is **derived
/// from it** — the `X-Multiview-Apply` header claims `live` for network kinds
/// exactly when a real spawner backs the claim.
async fn serve_control_plane(
    listen: &str,
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    wiring: ControlPlaneWiring,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<(
    tokio::task::JoinHandle<std::io::Result<()>>,
    multiview_control::CommandReceiver,
    multiview_cli::live_sources::LiveSourceHub,
    multiview_cli::config_watch::ConfigWatchHandle,
    RunningPersist,
)> {
    let config = &start.running;
    let ControlPlaneWiring {
        program_slot,
        stores,
        registry,
        ingest,
        live_apply,
    } = wiring;
    let (commands, command_rx) = command_bus(64);
    // The honesty keystone (ADR-W018): network kinds are declared live-appliable
    // exactly when the hub below carries a real ingest spawner.
    let live_apply = live_apply.with_sources(if ingest.is_some() {
        multiview_control::LiveSourceCapability::synthetic_and_network()
    } else {
        multiview_control::LiveSourceCapability::synthetic_only()
    });
    // The live-source hub (ADR-W018): owns runtime producer spawn/teardown +
    // the SHARED, live-updatable preview store map, off the clock thread.
    let shared_stores = multiview_cli::live_sources::shared_stores(stores);
    let hub = multiview_cli::live_sources::LiveSourceHub::start_with_ingest(
        registry,
        Arc::clone(&shared_stores),
        ingest,
    );
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
        live_apply,
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
    // baseline is the RUNNING document (the resumed one under resume), and
    // the boot-load TEXT seeds the last-observed content (review m4: the
    // unchanged boot file must never clobber a resumed baseline; an edit in
    // the boot window differs from this text and still applies).
    let watch = multiview_cli::config_watch::spawn(
        config_path.to_path_buf(),
        config.clone(),
        state,
        multiview_cli::config_watch::WatchOptions::default()
            .with_initial_observed(start.boot_text.clone()),
    );
    Ok((handle, command_rx, hub, watch, persist))
}

/// Wire the control plane for the full-pipeline run, when `[control]` is
/// configured: declare what THIS build + run path can take live (ADR-W018
/// sources via the pipeline's real ingest spawner; ADR-W021 overlays iff the
/// `overlay`-featured bake consumer renders them), serve the plane (with the
/// run's ADR-W022 Boot/Loaded/Running model + Running persister), and build
/// the frame-boundary command drain over the run's live seams. Without a
/// `[control]` section it returns the no-op drain (no server, no hub).
#[cfg(feature = "ffmpeg")]
#[allow(clippy::type_complexity)]
// reason: the one-shot wiring tuple of this run path's control-plane handles
// (server task, drain, hub, watch, persister), each `Option`al on whether
// `[control]` is configured. A named struct would be used exactly once.
async fn wire_pipeline_control_plane(
    start: &multiview_cli::boot::StartConfig,
    config_path: &Path,
    publisher: &Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    preview_slot: &multiview_cli::preview::ProgramSlot,
    pipeline: &multiview_cli::pipeline::Pipeline,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<(
    Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    ControlDrain,
    Option<multiview_cli::live_sources::LiveSourceHub>,
    Option<multiview_cli::config_watch::ConfigWatchHandle>,
    Option<RunningPersist>,
)> {
    let config = &start.running;
    let Some(cfg) = config.control.as_ref() else {
        drop(shutdown_rx);
        return Ok((
            None,
            Box::new(|_d: &mut CompositorDrive<Nv12Image>| {}),
            None,
            None,
            None,
        ));
    };
    // What THIS build + run path can take live (ADR-W021): with the
    // `overlay` feature the bake consumer renders the overlay working
    // set, so overlay documents the renderer draws (analog-face
    // clocks — `live_overlays::renders_live`, the same predicate the
    // drain warns by) apply live; without it nothing overlay-side
    // renders and the honest default (everything `restart`) stands.
    #[cfg(feature = "overlay")]
    let live_apply = multiview_control::LiveApplyCaps::default().with_overlays(
        multiview_control::OverlayLiveCapability::new(multiview_cli::live_overlays::renders_live),
    );
    #[cfg(not(feature = "overlay"))]
    let live_apply = multiview_control::LiveApplyCaps::default();
    let (handle, command_rx, hub, watch, persist) = serve_control_plane(
        &cfg.listen,
        start,
        config_path,
        publisher,
        ControlPlaneWiring {
            program_slot: Arc::clone(preview_slot),
            stores: pipeline.preview_stores(),
            registry: pipeline.stop_registry(),
            // The real decoded-ingest spawner (ADR-W018 level 2):
            // network/file sources live-apply through the SAME
            // supervised ingest_loop the startup path builds.
            ingest: Some(pipeline.live_ingest_spawner()),
            live_apply,
        },
        shutdown_rx,
    )
    .await?;
    // Thread the run's live subtitle re-point seam (RT-10b) into the drain so a
    // `RouteSubtitle` (RT-11) reaches the running pipeline's layer. The slot is
    // shared (lock-free `ArcSwapOption`); the run publishes its handle into it at
    // drive start, and the drain reads it wait-free (inv #1/#10). Only under
    // `overlay` (without it the run renders no subtitles, so there is no layer).
    // The live overlay seam (ADR-W021) rides the same variant; the
    // live-source seam (ADR-W018) rides both.
    #[cfg(feature = "overlay")]
    let drain: ControlDrain = Box::new(control::command_drain_with_seams(
        command_rx,
        config.clone(),
        Arc::clone(publisher),
        pipeline.subtitle_route_slot(),
        pipeline.overlay_apply_slot(),
        hub.handle(),
    ));
    #[cfg(not(feature = "overlay"))]
    let drain: ControlDrain = Box::new(control::command_drain_with_live_sources(
        command_rx,
        config.clone(),
        Arc::clone(publisher),
        hub.handle(),
    ));
    Ok((Some(handle), drain, Some(hub), Some(watch), Some(persist)))
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
    let (server, drain, live_hub, config_watch, running_persist) = wire_pipeline_control_plane(
        start,
        config_path,
        &publisher,
        &preview_slot,
        &pipeline,
        shutdown_rx,
    )
    .await?;

    // Review m2: Ctrl-C AND (on Unix) SIGTERM both run the graceful teardown,
    // so `docker stop`/systemd shutdowns capture the final `active.toml` too.
    let signal = spawn_stop_on_signal(stop.clone());

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

    // DEV-C1 (ADR-M010): the ~1 Hz outbound presentation-epoch publisher — one
    // `WallClockRef` per program as conflated `timing.status` on the control WS
    // plus the shared HLS-PDT cell every HLS sink stamps from. It binds lazily
    // to the run's tick-0 anchor (published when the clock seeds) and never
    // touches the engine (inv #1/#10); it self-stops on the run's StopSignal.
    let timing_cfg = config.timing.clone().unwrap_or_default();
    let timing_task = multiview_cli::timing_status::spawn(
        Arc::clone(&publisher),
        pipeline.epoch_anchor_slot(),
        pipeline.shared_epoch(),
        multiview_cli::timing_status::TimingStatusOptions {
            stream_id: multiview_config::ProgramId::MAIN.to_owned(),
            link_offset_ns: timing_cfg.link_offset_ns(),
            ptp_phc: timing_cfg.ptp_phc.clone(),
            ptp_utc_offset_ns: timing_cfg.ptp_utc_offset_ns(),
        },
        stop.clone(),
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
    // the same drive/stop/publisher/preview/drain). This path observes the live
    // tick counter only through the set; the node path passes its own shared
    // counter for the sd_notify watchdog.
    let report = drive_main_program_in_set(
        pipeline,
        cadence,
        &stop,
        &publisher,
        &preview_slot,
        drain,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
    )
    .await?;

    // The pipeline loop returned; stop the metrics + timing pollers (both also
    // self-stop on the StopSignal within one sample period), the config-file
    // watcher, the Running persister (with one final best-effort `active.toml`
    // capture — ADR-W022 §3), and tear down the live-source hub (it stops +
    // joins every runtime producer).
    metrics_task.abort();
    timing_task.abort();
    if let Some(watch) = config_watch {
        watch.stop();
    }
    if let Some(persist) = running_persist {
        persist.finish().await;
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
/// and samples its **live** `ticks_emitted` off the caller-supplied shared counter
/// the pipeline increments per tick — exactly the N-concurrent-programs machinery,
/// exercised here at N=1. MP-5 routes the config's `[[programs]]` into the same
/// `ProgramSet::start` for N>1. The caller keeps a clone of `main_ticks` where it
/// needs the live count itself (the node's `sd_notify` watchdog gates its liveness
/// pings on this counter advancing — DEV-B5).
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
    main_ticks: Arc<std::sync::atomic::AtomicU64>,
) -> anyhow::Result<multiview_cli::pipeline::PipelineReport> {
    // The shared monotonic reference every program in the set reads (its one program
    // reads it for its own clock's seed; identical to the inline `Monotonic` source
    // the pipeline built before).
    let mut programs: ProgramSet<RealtimePacer> =
        ProgramSet::new(Arc::new(multiview_engine::MonotonicTimeSource::new()));
    let program_id = ProgramId::new(ProgramId::MAIN).context("the reserved \"main\" program id")?;
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
            ControlPlaneWiring {
                program_slot: engine.program_preview(),
                stores: engine.preview_stores(),
                registry: engine.stop_registry(),
                // The software engine has no decoder: no ingest spawner, so
                // the capability (and the apply header) honestly stays
                // synthetic-only.
                ingest: None,
                // The software engine has no bake stage: no overlay
                // document renders on this path, so the honest default
                // (everything `restart`) is the truth (ADR-W021).
                live_apply: multiview_control::LiveApplyCaps::default(),
            },
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

    // Review m2: Ctrl-C AND (on Unix) SIGTERM both run the graceful teardown,
    // so `docker stop`/systemd shutdowns capture the final `active.toml` too.
    let signal = spawn_stop_on_signal(stop.clone());

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

    // DEV-C1 (ADR-M010): the outbound presentation epoch publishes on the
    // software path too — `timing.status` per program on the same drop-oldest
    // broadcast. The software run has no HLS sinks, so its epoch cell has no
    // PDT consumer; the WS surface is identical to the full-pipeline path.
    let timing_cfg = config.timing.clone().unwrap_or_default();
    let timing_task = multiview_cli::timing_status::spawn(
        Arc::clone(&publisher),
        engine.epoch_anchor_slot(),
        multiview_output::SharedEpoch::new(),
        multiview_cli::timing_status::TimingStatusOptions {
            stream_id: multiview_config::ProgramId::MAIN.to_owned(),
            link_offset_ns: timing_cfg.link_offset_ns(),
            ptp_phc: timing_cfg.ptp_phc.clone(),
            ptp_utc_offset_ns: timing_cfg.ptp_utc_offset_ns(),
        },
        stop.clone(),
    );

    let report = engine
        .run_until_stopped_with_control(&stop, publisher.as_ref(), drain)
        .await
        .context("headless run until Ctrl-C")?;

    // The engine loop returned; stop the metrics + timing pollers (both also
    // self-stop on the StopSignal within one sample period), the config-file
    // watcher, the Running persister (with one final best-effort `active.toml`
    // capture — ADR-W022 §3), tear down the live-source hub (it stops + joins
    // every runtime producer), and bring the control server down.
    metrics_task.abort();
    timing_task.abort();
    if let Some(watch) = config_watch {
        watch.stop();
    }
    if let Some(persist) = running_persist {
        persist.finish().await;
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
/// Returns the raw text alongside the parsed document — the text seeds the
/// ADR-W020 watcher's last-observed content (review m4).
fn load_validated(path: &Path) -> anyhow::Result<(String, MultiviewConfig)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config = MultiviewConfig::load_from_toml(&text)
        .with_context(|| format!("parsing config {}", path.display()))?;
    config
        .validate()
        .with_context(|| format!("validating config {}", path.display()))?;
    Ok((text, config))
}
