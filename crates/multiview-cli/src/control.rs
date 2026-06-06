//! Wiring the management control plane into `multiview run`.
//!
//! When the loaded config carries a `[control]` section, the run path binds that
//! address and serves the [`multiview_control`] router — REST + WebSocket + SSE,
//! the `OpenAPI`/Scalar docs at `/docs`, and (when the control plane is built with
//! `embed-web`) the web UI — alongside the engine, via
//! [`bind_and_serve`]. The server is a best-effort sibling task: it only reads
//! the engine's wait-free latest-state slot and drop-oldest event broadcast and
//! submits to the non-blocking command bus, so it is **physically incapable of
//! back-pressuring the engine** (invariant #10). It drains and stops gracefully
//! when the same shutdown signal the engine watches is raised.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{
    provision_admin_keys, AppState, Command, CommandReceiver, CommandSender, EngineStateSnapshot,
    InMemoryRepository, SharedPreview,
};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::{Event, OutputRunState, OutputStatus, SalvoEvent, SalvoPhase, TallyEvent};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Bind `listen` and serve the control plane over it on a background task,
/// shutting down gracefully when `shutdown` resolves.
///
/// Returns the **actual** bound [`SocketAddr`] (so a `:0` ephemeral bind can be
/// logged, or used by a test) and the server task's [`JoinHandle`]. The server
/// shares the engine's outbound `publisher` (read-only: the wait-free state slot
/// and drop-oldest event broadcast) and the inbound, non-blocking `commands` bus,
/// neither of which can stall the engine (invariant #10).
///
/// Access is provisioned with a bootstrap **admin** key
/// ([`provision_admin_keys`]): the unauthenticated surface (`/docs`,
/// `/api/v1/openapi.json`, and — with `embed-web` — the web UI shell) is always
/// reachable, while every API route requires the admin token. The admin secret
/// comes from the `MULTIVIEW_CONTROL_TOKEN` environment variable (stable across
/// restarts, no secret in config); if unset, a random token is generated and
/// **logged once** for first access. Finer-grained config-declared keys/roles
/// are a follow-up.
///
/// # Errors
/// Returns any I/O error from binding the `listen` address.
pub async fn bind_and_serve<F>(
    listen: &str,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
    commands: CommandSender,
    preview: SharedPreview,
    shutdown: F,
) -> std::io::Result<(SocketAddr, JoinHandle<std::io::Result<()>>)>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = TcpListener::bind(listen).await?;
    let addr = listener.local_addr()?;

    // Admin secret from the environment (12-factor; never from the repo/config),
    // else a generated bootstrap token surfaced once below.
    let admin_secret = std::env::var("MULTIVIEW_CONTROL_TOKEN")
        .ok()
        .filter(|s| !s.is_empty());
    let (api_keys, bootstrap_token) = provision_admin_keys(admin_secret);
    if let Some(token) = bootstrap_token {
        tracing::warn!(
            token = %token,
            "no MULTIVIEW_CONTROL_TOKEN set — generated a bootstrap admin token \
             (use as `Authorization: Bearer <token>`); set MULTIVIEW_CONTROL_TOKEN \
             to a stable secret for production"
        );
    } else {
        tracing::info!("control admin key provisioned from MULTIVIEW_CONTROL_TOKEN");
    }

    let state = AppState::new(
        publisher,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(api_keys),
    )
    .with_preview(preview);
    let handle = tokio::spawn(multiview_control::serve(listener, state, shutdown));
    Ok((addr, handle))
}

/// Project a composited program frame into the compact JSON snapshot the control
/// plane republishes from the wait-free latest-state slot (`EngineStateSnapshot`
/// is an opaque `serde_json::Value`, so the engine state shape stays decoupled
/// from the control plane). Kept intentionally small — schema tag, tick, output
/// PTS, and canvas geometry — so the per-tick serialization stays cheap on the
/// hot loop. Richer per-tile state is fed sparsely over the event stream as it
/// changes, not dumped here every frame.
#[must_use]
pub fn state_snapshot(tick: u64, pts_ns: i64, width: u32, height: u32) -> EngineStateSnapshot {
    serde_json::json!({
        "v": 1,
        "tick": tick,
        "pts_ns": pts_ns,
        "canvas": { "width": width, "height": height },
    })
}

/// Rebind the cell identified by `tile` to source `source` in `config`, in place.
///
/// Returns `true` if a cell with that id existed and was rebound (so the caller
/// re-solves + applies), `false` if no such cell — an unknown tile id is ignored
/// rather than an error (the command simply has no effect). The new binding is
/// validated downstream by [`MultiviewConfig::solve_layout`], so a `source` that
/// is not a declared input is rejected there (the layout is never swapped to an
/// invalid one).
fn apply_swap_source(config: &mut MultiviewConfig, tile: &str, source: &str) -> bool {
    let Some(cell) = config.cells.iter_mut().find(|c| c.id == tile) else {
        return false;
    };
    cell.source.input_id = Some(source.to_owned());
    cell.source.kind = None;
    cell.source.name = None;
    cell.source.url = None;
    true
}

/// Re-solve the working `config` and hot-swap it onto `drive`, returning `true`
/// on a successful apply.
///
/// Mirrors the existing [`Command::SwapSource`] apply path: a re-solve failure or
/// a compositor rejection logs `tracing::warn!` and keeps the last-good layout
/// (`set_layout` retains it on error), so the output clock never adopts a bad one
/// and never stalls (invariants #1 + #10). Panic-free: no `unwrap`/indexing.
fn resolve_and_apply(config: &MultiviewConfig, drive: &mut CompositorDrive<Nv12Image>) -> bool {
    match config.solve_layout() {
        Ok(layout) => match drive.set_layout(Arc::new(layout)) {
            Ok(()) => true,
            Err(e) => {
                // The compositor rejected the re-solved layout; keep the
                // last-good one (set_layout retains it on error) and log.
                tracing::warn!(error = %e, "rejected a control-plane layout swap");
                false
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "control-plane command produced an invalid layout; ignored");
            false
        }
    }
}

/// Apply a salvo's source recalls to the working `config` in place, returning
/// `true` if at least one cell was rebound (so the caller re-solves + applies).
///
/// Honest scope: of a [`Salvo`](multiview_config::Salvo)'s four sub-recalls, this
/// applies **only** `sources` (the cell→input rebindings). The `layout` preset is
/// not applied because the software engine has no named-layout library (the
/// config carries a single working layout, named `schema_v{N}`), and `tally`/`umd`
/// are not applied because there is no tally arbiter / UMD renderer wired into the
/// software engine yet. Those are a follow-up (CTL-5 / the tally arbiter). An
/// unknown cell in a recall is ignored (no effect), never an error.
fn apply_salvo_sources(config: &mut MultiviewConfig, salvo: &multiview_config::Salvo) -> bool {
    let mut changed = false;
    for recall in &salvo.sources {
        if apply_swap_source(config, &recall.cell, &recall.input_id) {
            changed = true;
        }
    }
    changed
}

/// Build the engine's per-tick control hook that drains the command bus and
/// applies operational commands to the running compositor at the frame boundary,
/// emitting each command's outcome on the realtime event stream.
///
/// Returned as an `FnMut(&mut CompositorDrive<Nv12Image>)` for the engine's
/// per-tick control drain: each tick it [`try_drain`](CommandReceiver::try_drain)s
/// the **non-blocking** queue (usually empty — O(pending), never awaits) and, for
/// each command, mutates the working [`MultiviewConfig`], re-solves + hot-swaps
/// the layout where applicable via [`CompositorDrive::set_layout`], and publishes
/// an outcome [`Event`] via [`EnginePublisher::publish_event`] — which is
/// **drop-oldest and never awaits a client**, so emitting an outcome can never
/// back-pressure the engine (invariant #10). Applying at the frame boundary keeps
/// the output clock unstalled (invariant #1): the drain only mutates the active
/// layout and emits drop-oldest events; it never blocks.
///
/// Per command:
/// * [`Command::Start`]/[`Command::Stop`] flip the closure's `running` flag and
///   emit an [`Event::OutputStatus`] (`Running` / `Idle`). There is no output
///   server wired in the software engine yet, so this is the running-state echo,
///   not a measured sink status.
/// * [`Command::SwapSource`] rebinds a tile and re-applies the layout (the prior
///   behaviour). No dedicated swap event exists in [`Event`], so the observable
///   outcome is the layout change plus a `tracing` log — deliberately **not** an
///   invented event variant.
/// * [`Command::ApplyLayout`] re-applies the working layout iff `layout` matches
///   the solved working layout's name; any other id is a failure (there is no
///   named-layout library yet) — logged via `tracing::warn!`, never a panic.
/// * [`Command::ArmSalvo`] stages a named salvo's source recalls and emits
///   [`Event::SalvoArmed`]; [`Command::TakeSalvo`] applies the named-or-armed
///   salvo's source recalls + re-applies the layout and emits
///   [`Event::SalvoTaken`]; [`Command::CancelSalvo`] discards the staged salvo and
///   emits [`Event::SalvoCancelled`]. Only the salvo's `sources` are applied (see
///   [`apply_salvo_sources`]); the layout/tally/umd sub-recalls are a follow-up.
/// * [`Command::SetTallyOverride`] has no tally arbiter in the software engine
///   yet, so it emits an [`Event::TallyState`] echo (the forced colour, or the
///   `Off`/default state when cleared) rather than silently no-op'ing.
///
/// Every arm is panic-free: no `unwrap`/`expect`/indexing. An unknown layout or
/// salvo logs `tracing::warn!` and emits nothing (or a tally echo), never panics.
pub fn command_drain(
    mut commands: CommandReceiver,
    mut config: MultiviewConfig,
    publisher: Arc<EnginePublisher<EngineStateSnapshot, Event>>,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    // Closure-captured state across ticks: whether program output is "running",
    // and the id of the currently-armed salvo (its source recalls are read from
    // `config` when the take applies). Bounded, allocation-light, touched only on
    // the (usually empty) per-tick drain.
    let mut state = DrainState::default();

    move |drive: &mut CompositorDrive<Nv12Image>| {
        for command in commands.try_drain() {
            apply_command(command, &mut state, &mut config, &publisher, drive);
        }
    }
}

/// Per-tick command-drain state retained across ticks.
#[derive(Debug, Default)]
struct DrainState {
    /// Whether program output is "running" (flipped by Start/Stop). Observed via
    /// the emitted `OutputStatus` events; retained so a future periodic-status
    /// republish can read it without re-deriving.
    running: bool,
    /// The id of the currently-armed salvo awaiting a take, if any.
    armed_salvo: Option<String>,
}

/// Apply one drained command to the working config + active layout and emit its
/// outcome event. Panic-free (no `unwrap`/`expect`/indexing); an unknown
/// layout/salvo logs `tracing::warn!` and emits nothing (or a tally echo).
fn apply_command(
    command: Command,
    state: &mut DrainState,
    config: &mut MultiviewConfig,
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    drive: &mut CompositorDrive<Nv12Image>,
) {
    match command {
        Command::Start { .. } => {
            state.running = true;
            publish_output_status(publisher, OutputRunState::Running);
        }
        Command::Stop { .. } => {
            state.running = false;
            publish_output_status(publisher, OutputRunState::Idle);
        }
        Command::SwapSource { tile, source, .. } => {
            if apply_swap_source(config, &tile, &source) {
                // No dedicated swap event exists; the observable outcome is the
                // re-applied layout (and the warn log on rejection).
                let _ = resolve_and_apply(config, drive);
            } else {
                tracing::warn!(tile = %tile, "swap_source: no such tile; ignored");
            }
        }
        Command::ApplyLayout { layout, .. } => {
            // There is no named-layout library in the software engine: the working
            // config carries a single solved layout named `schema_v{N}`. Applying
            // that name (the only valid id) re-solves + re-applies the working
            // layout; any other id is a failure (no panic).
            // FOLLOW-UP (CTL-4/CTL-2): resolve `layout` against a real layout
            // library once one exists.
            let working = config.solve_layout().ok().map(|l| l.name);
            if working.as_deref() == Some(layout.as_str()) {
                let _ = resolve_and_apply(config, drive);
            } else {
                tracing::warn!(
                    layout = %layout,
                    "apply_layout: unknown layout id (no named-layout library yet); ignored"
                );
            }
        }
        Command::ArmSalvo { salvo, head, .. } => {
            if config.salvos.iter().any(|s| s.id == salvo) {
                // Stage the salvo: its source recalls are read from `config` at
                // take time, so staging is just remembering the id.
                state.armed_salvo = Some(salvo.clone());
                publisher.publish_event(Event::SalvoArmed(salvo_event(
                    salvo,
                    SalvoPhase::Armed,
                    head,
                )));
            } else {
                tracing::warn!(salvo = %salvo, "arm_salvo: no such salvo; ignored");
            }
        }
        Command::TakeSalvo { salvo, head, .. } => {
            // Take the named salvo, else the currently-armed one.
            let Some(target) = salvo.or_else(|| state.armed_salvo.clone()) else {
                tracing::warn!("take_salvo: no salvo named and none armed; ignored");
                return;
            };
            // Clone the matched salvo out so the immutable borrow of `config` ends
            // before the in-place mutation below (avoids aliasing).
            let Some(recalled) = config.salvos.iter().find(|s| s.id == target).cloned() else {
                tracing::warn!(salvo = %target, "take_salvo: no such salvo; ignored");
                return;
            };
            if apply_salvo_sources(config, &recalled) {
                let _ = resolve_and_apply(config, drive);
            }
            state.armed_salvo = None;
            publisher.publish_event(Event::SalvoTaken(salvo_event(
                target,
                SalvoPhase::Taken,
                head,
            )));
        }
        Command::CancelSalvo { salvo, head, .. } => {
            // Cancel the named salvo, else the currently-armed one.
            let target = salvo.or_else(|| state.armed_salvo.clone());
            state.armed_salvo = None;
            let Some(target) = target else {
                tracing::warn!("cancel_salvo: no salvo named and none armed; ignored");
                return;
            };
            publisher.publish_event(Event::SalvoCancelled(salvo_event(
                target,
                SalvoPhase::Cancelled,
                head,
            )));
        }
        Command::SetTallyOverride { target, color, .. } => {
            // No tally arbiter is wired into the software engine yet, so this emits
            // a TallyState echo rather than silently no-op'ing: a forced colour maps
            // to a program-bus lamp of that colour at the default brightness; a
            // cleared override (`None`) maps to the unlit default state.
            // FOLLOW-UP: route through the real arbiter once it exists.
            let state = match color {
                Some(color) => multiview_core::tally::TallyState {
                    color,
                    ..multiview_core::tally::TallyState::default()
                },
                None => multiview_core::tally::TallyState::default(),
            };
            publisher.publish_event(Event::TallyState(TallyEvent { target, state }));
        }
        // `Command` is `#[non_exhaustive]`: a future variant this build does not
        // know about is logged and skipped, never panicked on.
        ref other => {
            tracing::warn!(kind = other.kind(), "unhandled control command; skipped");
        }
    }
}

/// Emit an `OutputStatus` event with no measured bitrate/client count (the
/// software engine has no output server wired in yet — this is the running-state
/// echo, not a measured sink status).
fn publish_output_status(
    publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    run_state: OutputRunState,
) {
    publisher.publish_event(Event::OutputStatus(OutputStatus {
        state: run_state,
        bitrate_bps: None,
        clients: None,
    }));
}

/// Build a `SalvoEvent` for `salvo` entering `phase`, scoped to `head` if given.
fn salvo_event(salvo: String, phase: SalvoPhase, head: Option<String>) -> SalvoEvent {
    let event = SalvoEvent::new(salvo, phase);
    match head {
        Some(head) => event.with_head(head),
        None => event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_compositor::blend::LinearRgba;
    use multiview_compositor::pipeline::CanvasColor;
    use multiview_control::{command_bus, Command, OperationId};
    use multiview_engine::EnginePublisher;
    use multiview_events::{Event, OutputRunState};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A two-source, two-cell config carrying one salvo whose source recall
    /// rebinds `cell_a` from its config-default `in_a` to `in_b`.
    const TWO_CELL_DOC: &str = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "rtsp"
url = "rtsp://x/a"
[[sources]]
id = "in_b"
kind = "rtsp"
url = "rtsp://x/b"
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[salvos]]
id = "salvo_one"
[[salvos.sources]]
cell = "cell_a"
input_id = "in_b"
"##;

    fn test_config() -> MultiviewConfig {
        MultiviewConfig::load_from_toml(TWO_CELL_DOC).expect("parse two-cell config")
    }

    /// Build a real `CompositorDrive` over the test config's solved layout, with
    /// no frame stores (every tile shows the slate — irrelevant to these tests,
    /// which only assert layout/event effects of the drain).
    fn test_drive(config: &MultiviewConfig) -> CompositorDrive<Nv12Image> {
        let layout = config.solve_layout().expect("solve layout");
        let canvas_color = CanvasColor::default();
        let nosignal = Nv12Image::solid(
            config.canvas.width,
            config.canvas.height,
            16,
            128,
            128,
            canvas_color.output_tag(),
        )
        .expect("nosignal card");
        CompositorDrive::new(
            Arc::new(layout),
            std::collections::HashMap::new(),
            nosignal,
            canvas_color,
            LinearRgba::opaque(0.0, 0.0, 0.0),
        )
        .expect("build drive")
    }

    /// The core-cell index whose source binding is `want`, if any.
    fn cell_index_bound_to(drive: &CompositorDrive<Nv12Image>, want: &str) -> Option<usize> {
        drive
            .layout()
            .cells
            .iter()
            .position(|c| c.source.as_deref() == Some(want))
    }

    #[test]
    fn start_then_stop_emits_output_status() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut sub = publisher.subscribe();
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        sender
            .try_submit(Command::Start {
                op: OperationId::new(),
            })
            .expect("submit start");
        sender
            .try_submit(Command::Stop {
                op: OperationId::new(),
            })
            .expect("submit stop");
        drain(&mut drive);

        let first = sub.try_recv().expect("first event present");
        match first.event.as_ref() {
            Event::OutputStatus(s) => assert_eq!(s.state, OutputRunState::Running),
            other => panic!("expected Running OutputStatus, got {other:?}"),
        }
        let second = sub.try_recv().expect("second event present");
        match second.event.as_ref() {
            Event::OutputStatus(s) => assert_eq!(s.state, OutputRunState::Idle),
            other => panic!("expected Idle OutputStatus, got {other:?}"),
        }
    }

    #[test]
    fn apply_layout_swaps_active_layout() {
        let config = test_config();
        let working_name = config.solve_layout().expect("solve").name;
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Applying the working layout name re-solves and re-applies successfully:
        // the active layout keeps its (stable) name and is unchanged.
        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: working_name.clone(),
            })
            .expect("submit apply-layout");
        drain(&mut drive);

        assert_eq!(drive.layout().name, working_name);
        assert_eq!(drive.layout().cells.len(), 2);
    }

    #[test]
    fn unknown_layout_emits_failure_not_panic() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut sub = publisher.subscribe();
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());
        let before = drive.layout().name.clone();

        sender
            .try_submit(Command::ApplyLayout {
                op: OperationId::new(),
                layout: "no_such_layout".to_owned(),
            })
            .expect("submit apply-layout");
        // Must not panic.
        drain(&mut drive);

        // The active layout is untouched by an unknown layout id.
        assert_eq!(drive.layout().name, before);
        // No spurious success: no `OutputStatus` (a successful apply does not emit
        // one anyway) and specifically no salvo/tally event is emitted here. The
        // only thing on the stream, if anything, must not claim success — assert
        // there is no event at all.
        assert!(
            sub.try_recv().is_err(),
            "an unknown layout must not emit a success event"
        );
    }

    #[test]
    fn salvo_take_applies_armed_layout() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
        let (sender, command_rx) = command_bus(8);
        let mut sub = publisher.subscribe();
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Before: cell_a (index 0) is bound to in_a; cell_b (index 1) to in_b.
        assert_eq!(
            drive.layout().cells.first().and_then(|c| c.source.clone()),
            Some("in_a".to_owned())
        );

        sender
            .try_submit(Command::ArmSalvo {
                op: OperationId::new(),
                salvo: "salvo_one".to_owned(),
                head: None,
            })
            .expect("submit arm");
        sender
            .try_submit(Command::TakeSalvo {
                op: OperationId::new(),
                salvo: None,
                head: None,
            })
            .expect("submit take");
        drain(&mut drive);

        // The salvo rebinds cell_a's source to in_b; both cells now show in_b.
        assert_eq!(
            drive.layout().cells.first().and_then(|c| c.source.clone()),
            Some("in_b".to_owned()),
            "salvo take must rebind cell_a to in_b"
        );
        // Both cell indices are now bound to in_b (cell_b already was).
        assert!(cell_index_bound_to(&drive, "in_a").is_none());

        // Arm and Take each emit their salvo lifecycle event.
        let armed = sub.try_recv().expect("armed event");
        assert!(
            matches!(armed.event.as_ref(), Event::SalvoArmed(e) if e.salvo == "salvo_one"),
            "expected SalvoArmed, got {:?}",
            armed.event
        );
        let taken = sub.try_recv().expect("taken event");
        assert!(
            matches!(taken.event.as_ref(), Event::SalvoTaken(e) if e.salvo == "salvo_one"),
            "expected SalvoTaken, got {:?}",
            taken.event
        );
    }

    #[test]
    fn drain_is_bounded_and_never_awaits() {
        let config = test_config();
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (sender, command_rx) = command_bus(64);
        let mut drain = command_drain(command_rx, config, Arc::clone(&publisher));
        let mut drive = test_drive(&test_config());

        // Flood the bus with a mix of accepted commands.
        for _ in 0..16 {
            sender
                .try_submit(Command::Start {
                    op: OperationId::new(),
                })
                .expect("submit start");
            sender
                .try_submit(Command::SwapSource {
                    op: OperationId::new(),
                    tile: "cell_a".to_owned(),
                    source: "in_b".to_owned(),
                })
                .expect("submit swap");
        }

        // The drain is a synchronous `FnMut`: calling it processes every pending
        // command in O(pending) and returns without awaiting anything. A second
        // call over the now-empty bus is a no-op and also returns.
        drain(&mut drive);
        drain(&mut drive);

        // The swaps took effect (cell_a now bound to in_b) — proof the loop ran
        // to completion rather than blocking.
        assert_eq!(
            drive.layout().cells.first().and_then(|c| c.source.clone()),
            Some("in_b".to_owned())
        );
    }

    #[test]
    fn state_snapshot_is_compact_and_tagged() {
        let snap = state_snapshot(7, 233_333_333, 1920, 1080);
        assert_eq!(snap["v"], 1);
        assert_eq!(snap["tick"], 7);
        assert_eq!(snap["pts_ns"], 233_333_333_i64);
        assert_eq!(snap["canvas"]["width"], 1920);
        assert_eq!(snap["canvas"]["height"], 1080);
    }

    /// `bind_and_serve` binds a real loopback socket, serves the unauthenticated
    /// `OpenAPI` document, and returns cleanly once its shutdown future resolves.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_and_serve_exposes_openapi_then_shuts_down() {
        let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
        let (commands, _rx) = multiview_control::command_bus(8);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let (addr, handle) = bind_and_serve(
            "127.0.0.1:0",
            publisher,
            commands,
            multiview_control::no_preview(),
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .expect("bind + serve should start");

        // A genuine client hits the unauthenticated OpenAPI document (the control
        // plane's default `openapi` feature). HTTP/1.0 + close → read to EOF.
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /api/v1/openapi.json HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let response = String::from_utf8_lossy(&buf);
        // Assert the status CODE (the second token), not the protocol version —
        // hyper may answer an HTTP/1.0 request as 1.0 or 1.1.
        let status_line = response.lines().next().unwrap_or_default();
        assert_eq!(
            status_line.split_whitespace().nth(1),
            Some("200"),
            "expected a 200 status code, got status line: {status_line:?}"
        );
        assert!(
            response.contains("openapi"),
            "expected an OpenAPI document in the body"
        );

        // Graceful shutdown returns cleanly within a generous bound.
        shutdown_tx.send(()).unwrap();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("serve should return within 5s of shutdown");
        joined
            .expect("serve task panicked")
            .expect("serve returned an I/O error");
    }
}
