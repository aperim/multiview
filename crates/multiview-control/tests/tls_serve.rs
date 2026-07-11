//! TLS-0 (ADR-W029) end-to-end: the *served* control plane terminates rustls TLS
//! over the shared listener AND preserves the SEC-14 per-IP rate limiter
//! (`ConnectInfo`) through `axum-server`.
//!
//! This mirrors `management_limits.rs` (the plain-HTTP SEC-14 e2e) but over
//! HTTPS: a real `reqwest` client completes the rustls handshake against the
//! served listener, and a per-IP burst of 1 proves the second request from the
//! same source IP is rejected `429` — i.e. the peer `SocketAddr` reached the
//! pre-auth guard through the TLS accept loop
//! (`into_make_service_with_connect_info`), the load-bearing SEC-14 invariant
//! the team-lead flagged must not regress under TLS.
//!
//! Gated `required-features = ["tls"]` (see `Cargo.toml`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_config::limits::ManagementLimits;
use multiview_config::TlsConfig;
use multiview_control::{
    command_bus, load_tls_material, serve_tls, ApiKeyStore, AppState, EngineStateSnapshot,
    InMemoryRepository,
};
use multiview_engine::EnginePublisher;
use multiview_events::Event;
use tokio::net::TcpListener;

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
        Arc::new(ApiKeyStore::new(b"tls-e2e-pepper".to_vec())),
    )
    .with_limits(&limits)
}

/// Generate an ephemeral self-signed certificate + key and write them as PEM into
/// `dir`, returning `(cert_path, key_path)`. The client trusts nothing
/// (`danger_accept_invalid_certs`), so the SANs are irrelevant — only that the
/// server presents a real, loadable certificate.
fn self_signed_pem(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("generate a self-signed certificate");
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem()).expect("write cert.pem");
    std::fs::write(&key_path, signing_key.serialize_pem()).expect("write key.pem");
    (cert_path, key_path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_served_tls_control_plane_handshakes_and_rate_limits_per_ip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (cert, key) = self_signed_pem(dir.path());
    let tls_cfg = TlsConfig::Static { cert, key };

    // Load the operator's PEM material into a rustls server config (explicit
    // aws-lc-rs provider). A load failure here is a hard startup error in the
    // binary; here it must succeed for a well-formed self-signed pair.
    let material = load_tls_material(&tls_cfg).expect("load self-signed TLS material");

    // IPv6-first loopback bind (conventions §10).
    let listener = TcpListener::bind("[::1]:0").await.expect("bind [::1]:0");
    let addr = listener.local_addr().expect("local_addr");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve_tls(listener, limited_state(), material, async move {
        let _ = shutdown_rx.await;
    }));

    // A REAL HTTPS client. `danger_accept_invalid_certs` because the cert is an
    // ephemeral self-signed one — we are proving the handshake + the limiter, not
    // PKI. If the rustls termination were broken, `send()` would error here.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("build reqwest client");
    let url = format!("https://[::1]:{}/api/v1/openapi.json", addr.port());

    // First request from the loopback client is within the per-IP burst of 1.
    let first = client.get(&url).send().await.expect("first HTTPS GET");
    assert_eq!(
        first.status().as_u16(),
        200,
        "first TLS request should be admitted (proves the rustls handshake completed)"
    );

    // The second request (same source IP, immediately) exceeds the burst → 429.
    // Proves the peer IP reached the SEC-14 per-IP guard over the TLS accept loop.
    let second = client.get(&url).send().await.expect("second HTTPS GET");
    assert_eq!(
        second.status().as_u16(),
        429,
        "second TLS request from the same IP should be rate-limited (ConnectInfo preserved \
         through axum-server)"
    );

    let _ = shutdown_tx.send(());
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve_tls returned within 5s of shutdown")
        .expect("serve_tls task did not panic")
        .expect("serve_tls returned no I/O error");
}
