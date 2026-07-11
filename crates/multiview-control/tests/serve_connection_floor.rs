//! The control-plane serve loop is **HTTP/1-only** and enforces an accept-level
//! connection population cap (SEC-14 #126 R2 / [ADR-W031](../../docs/decisions/ADR-W031.md)).
//!
//! These are the round-2 hardening tests on top of the header-read timeout
//! (`serve_header_timeout.rs`):
//!
//! * **F1 — HTTP/2 is refused.** The header-read timeout only bounds HTTP/1 header
//!   reads; hyper's HTTP/2 server has no equivalent, so an HTTP/2 connection (which
//!   the old `auto` builder would negotiate on the h2 preface) could pin a slot
//!   forever. Serving on `hyper::server::conn::http1::Builder` subjects *every*
//!   connection — the h2 preface included — to the header-read timeout, so a slot
//!   can never be held open past the deadline.
//! * **F2 — accept-level population cap.** A flood of half-open connections is bounded
//!   at accept (global + per-IP), before any request headers parse.
//! * **F3 — bounded drain.** At shutdown, in-flight connection tasks are tracked and
//!   aborted after a ceiling, so none outlives `serve`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use http_body::{Body as HttpBody, Frame};
use multiview_control::{
    command_bus, ApiKeyStore, AppState, EngineStateSnapshot, InMemoryRepository, ServeOptions,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A short header-read deadline keeps the test fast; production defaults to 20 s.
const TEST_HEADER_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// The HTTP/2 connection preface a client sends to begin an h2 session over cleartext
/// (RFC 9113 §3.4). The old `auto` builder negotiates HTTP/2 on seeing this; an
/// HTTP/1-only serve loop treats it as a malformed request line instead.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

fn test_state() -> AppState {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"serve-connection-floor-pepper".to_vec())),
    )
}

/// Bind an ephemeral IPv6 loopback listener and serve the control plane with the
/// given [`ServeOptions`], returning the address + a shutdown sender + the join handle.
async fn serve_with(
    options: ServeOptions,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert!(addr.is_ipv6(), "control plane must bind IPv6 loopback");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve_with(
        listener,
        test_state(),
        options,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    (addr, shutdown_tx, server)
}

/// F1: an HTTP/2 client (one that sends the h2 preface, then stalls without ever
/// completing an HTTP/1 header block) must have its connection dropped at the
/// header-read deadline — never negotiated into a timeout-free HTTP/2 session that
/// pins the slot open forever. The header-read timeout only bounds HTTP/1 header
/// reads, so this is the whole point of serving HTTP/1-only: the preface is parsed as
/// a (bad) HTTP/1 request line and is therefore subject to the same deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_http2_preface_is_refused_not_held_open_as_a_timeout_free_session() {
    let options =
        ServeOptions::default().with_header_read_timeout(Some(TEST_HEADER_READ_TIMEOUT));
    let (addr, shutdown_tx, server) = serve_with(options).await;

    // Send the h2 connection preface and nothing else — an h2 server would reply with
    // a SETTINGS frame and then wait for the client's frames indefinitely (no
    // header-read timeout applies to h2), pinning the connection. An HTTP/1-only
    // server drops it at the header-read deadline instead.
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(H2_PREFACE).await.unwrap();

    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    let closed = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf)).await;
    let elapsed = start.elapsed();

    // The connection must be CLOSED (the read resolved) rather than held open until the
    // 5 s outer bound — the "h2 has no header-read timeout" failure. Under the old
    // `auto` builder the preface negotiates a live h2 session that never closes, so
    // this read hits the outer timeout (`closed.is_err()`).
    assert!(
        closed.is_ok(),
        "the server held an HTTP/2-preface connection open past the {TEST_HEADER_READ_TIMEOUT:?} \
         header-read deadline — HTTP/2 was negotiated and its slot pinned with no header timeout \
         (serve HTTP/1-only so the preface is bounded by the deadline). elapsed={elapsed:?}"
    );
    // And no working exchange happened: no HTTP/1 success and no live h2 session.
    let response = String::from_utf8_lossy(&buf);
    assert!(
        !response.contains("200"),
        "the server completed an exchange for an HTTP/2 preface: {response:?}"
    );

    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// F2: the accept-level global connection cap bounds the *population* of connections
/// the serve loop will hold at all — before any request headers parse. With the cap at
/// 2, two slow-header connections occupy both slots and a third is dropped promptly at
/// accept, rather than being admitted and held until its own header-read deadline (the
/// unbounded-population failure the request-level caps miss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_global_connection_cap_drops_over_cap_connections_at_accept() {
    // Generous header-read timeout so the two occupying connections stay held for the
    // whole test; the per-IP cap is set high so the GLOBAL cap of 2 is the binding one.
    let options = ServeOptions::default()
        .with_header_read_timeout(Some(Duration::from_secs(2)))
        .with_max_connections(Some(2))
        .with_max_connections_per_ip(Some(100));
    let (addr, shutdown_tx, server) = serve_with(options).await;

    // Occupy both global slots with slow-header connections (admitted at accept, then
    // held reading their unfinished header block).
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut s = TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /api/v1/openapi.json HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        held.push(s);
    }
    // Let the sequential accept loop admit + spawn both.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // The third connection is over the global cap of 2. The loop accepts the TCP
    // connection then immediately drops it (no admission), so the client sees a prompt
    // EOF — NOT a slot held open until the 2 s header deadline.
    let mut third = TcpStream::connect(addr).await.unwrap();
    let mut buf = Vec::new();
    let start = std::time::Instant::now();
    let read = tokio::time::timeout(Duration::from_millis(500), third.read_to_end(&mut buf)).await;
    let elapsed = start.elapsed();
    assert!(
        read.is_ok(),
        "the over-cap connection was still open after {elapsed:?} (held, not dropped at accept) — \
         the accept-level global cap is not enforced"
    );
    assert!(
        buf.is_empty(),
        "a connection dropped at accept returns no bytes; got {buf:?}"
    );

    drop(held);
    shutdown_tx.send(()).unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
}

/// A response body that never yields a frame and never ends — an in-flight response the
/// graceful drain cannot complete, forcing the abort path.
struct PendingBody;

impl HttpBody for PendingBody {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Pending
    }

    fn is_end_stream(&self) -> bool {
        false
    }
}

/// Bind an ephemeral IPv6 loopback listener and serve an arbitrary [`Router`] with the
/// given [`ServeOptions`].
async fn serve_router_with_opts(
    app: Router,
    options: ServeOptions,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<std::io::Result<()>>,
) {
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve_router_with(
        listener,
        app,
        options,
        async move {
            let _ = shutdown_rx.await;
        },
    ));
    (addr, shutdown_tx, server)
}

/// F3: an in-flight connection whose response never completes (a pending body, standing
/// in for a live WebSocket / SSE stream) must be ABORTED at the drain ceiling, so no
/// connection task outlives `serve`. Under the old detached-`tokio::spawn` loop the
/// task is abandoned (the connection stays alive) when the ceiling is reached; the
/// JoinSet + `abort_all` fix drops it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_flight_connection_is_aborted_at_the_drain_ceiling() {
    let app = Router::new().route(
        "/pending",
        get(|| async { Response::new(Body::new(PendingBody)) }),
    );
    let ceiling = Duration::from_millis(300);
    let options = ServeOptions::default()
        // Generous header-read timeout: this test is about the DRAIN, not the header
        // deadline — the request completes, only the response body pends.
        .with_header_read_timeout(Some(Duration::from_secs(5)))
        .with_graceful_shutdown_ceiling(ceiling);
    let (addr, shutdown_tx, server) = serve_router_with_opts(app, options).await;

    // Send a COMPLETE request for the never-ending response, then give the server a
    // moment to route it and enter the pending-body state (the connection is now
    // in-flight and will not drain gracefully).
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(b"GET /pending HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Signal shutdown: the in-flight connection cannot complete, so the loop must abort
    // it after the ceiling.
    shutdown_tx.send(()).unwrap();

    // serve() must return within the ceiling + slack (not hang on the stuck connection).
    let served = tokio::time::timeout(ceiling + Duration::from_secs(3), server).await;
    assert!(
        served.is_ok(),
        "serve() did not return within the drain ceiling after shutdown"
    );

    // The in-flight connection must be CLOSED shortly after the ceiling — abort_all
    // dropped its task. Under the leaked-detached-task bug the task keeps the pending
    // body alive and this read never resolves.
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut rest)).await;
    assert!(
        closed.is_ok(),
        "the in-flight connection was not aborted at the drain ceiling — a connection task \
         outlived serve() (F3 leak)"
    );
}
