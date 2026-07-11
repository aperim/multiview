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

use std::sync::Arc;
use std::time::Duration;

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
