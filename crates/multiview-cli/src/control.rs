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
    InMemoryRepository,
};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::Event;
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
    );
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

/// Build the engine's per-tick control hook that drains the command bus and
/// applies operational commands to the running compositor at the frame boundary.
///
/// Returned as an `FnMut(&mut CompositorDrive<Nv12Image>)` for
/// [`EngineRuntime::run_with_control`](multiview_engine::EngineRuntime::run_with_control):
/// each tick it [`try_drain`](CommandReceiver::try_drain)s the **non-blocking**
/// queue (usually empty — O(pending), never awaits) and, for each command it can
/// apply, mutates the working [`MultiviewConfig`], re-solves the layout, and
/// hot-swaps it via [`CompositorDrive::set_layout`]. A command that does not map
/// to a layout change (start/stop/salvo/tally) is left for the realtime/mirror
/// path and skipped here. Applying at the frame boundary, non-blocking, is what
/// keeps the output clock unstalled (invariants #1 + #10).
///
/// Currently handles [`Command::SwapSource`] (rebind a tile's source). The other
/// operational commands and the per-command outcome events are a follow-up.
pub fn command_drain(
    mut commands: CommandReceiver,
    mut config: MultiviewConfig,
) -> impl FnMut(&mut CompositorDrive<Nv12Image>) {
    move |drive: &mut CompositorDrive<Nv12Image>| {
        for command in commands.try_drain() {
            let changed = match command {
                Command::SwapSource { tile, source, .. } => {
                    apply_swap_source(&mut config, &tile, &source)
                }
                // Other commands don't (yet) drive a layout swap from here.
                _ => false,
            };
            if !changed {
                continue;
            }
            match config.solve_layout() {
                Ok(layout) => {
                    if let Err(e) = drive.set_layout(Arc::new(layout)) {
                        // The compositor rejected the re-solved layout; keep the
                        // last-good one (set_layout retains it on error) and log.
                        tracing::warn!(error = %e, "rejected a control-plane layout swap");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "control-plane command produced an invalid layout; ignored");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

        let (addr, handle) = bind_and_serve("127.0.0.1:0", publisher, commands, async move {
            let _ = shutdown_rx.await;
        })
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
