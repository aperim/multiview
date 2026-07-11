//! End-to-end (SEC-14): the *served* control plane installs the management-plane
//! connection + rate caps and keys the per-IP limit on the real peer address.
//!
//! The middleware behaviour is unit-tested in-process (`src/limits.rs`); this test
//! adds the real TCP bind + `into_make_service_with_connect_info` wiring, so it
//! proves `ConnectInfo` actually reaches the per-IP guard over a genuine socket
//! and that `router()` installs the layers when `control.limits` is enabled.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_config::limits::ManagementLimits;
use multiview_control::{
    command_bus, ApiKeyStore, AppState, EngineStateSnapshot, InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A real `AppState` with the SEC-14 limits enabled and a per-IP burst of exactly
/// one, so the second request from the same loopback client is rejected.
fn limited_state() -> AppState {
    let engine = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let (commands, _rx) = command_bus(8);
    let mut limits = ManagementLimits::default();
    limits.per_ip.burst = 1;
    limits.per_ip.refill_per_sec = 1;
    AppState::new(
        engine,
        commands,
        Arc::new(InMemoryRepository::new()),
        Arc::new(ApiKeyStore::new(b"limits-e2e-pepper".to_vec())),
    )
    .with_limits(&limits)
}

/// Minimal HTTP/1.0 client: one request, read to EOF, return the numeric status
/// code from the status line (`HTTP/<ver> <code> <reason>`).
async fn http_get_status(addr: std::net::SocketAddr, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).into_owned();
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|code| code.to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_served_control_plane_rate_limits_per_source_ip() {
    // IPv6-first loopback bind (conventions §10).
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(multiview_control::serve(listener, limited_state(), async move {
        let _ = shutdown_rx.await;
    }));

    // First request from the loopback client is within the per-IP burst of 1.
    assert_eq!(
        http_get_status(addr, "/api/v1/openapi.json").await.as_deref(),
        Some("200"),
        "first request should be admitted"
    );
    // The second request (same source IP, immediately) exceeds the burst → 429.
    // Proves ConnectInfo reached the per-IP guard over a real socket.
    assert_eq!(
        http_get_status(addr, "/api/v1/openapi.json").await.as_deref(),
        Some("429"),
        "second request from the same IP should be rate-limited"
    );

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve returned within 5s of shutdown")
        .expect("serve task did not panic")
        .expect("serve returned no I/O error");
}
