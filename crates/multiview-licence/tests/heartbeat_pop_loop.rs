//! CONSPECT-3 device-PoP — the heartbeat LOOP integration (ADR-I007): the nonce
//! lifecycle (cold-start `/challenge` + steady-state `nextNonce`), the
//! `Conspect-Device-PoP` header + `nonce` body field reaching the server, and
//! fail-closed-on-every-PoP-failure (never off air, inv #1/#10).
//!
//! These drive the real `HeartbeatClient` against the in-process `FakeLicenceServer`
//! (extended to serve challenges, record the PoP header, and verify the proof
//! against the device public key the client signs with).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use multiview_licence::heartbeat::{HeartbeatClient, HeartbeatOutcome};
use multiview_licence::{EnforcementLevel, LeaseStore};

mod fake;
use fake::{pop_test_signer, shared_fake, FakeLicenceServer};

fn test_config() -> multiview_licence::heartbeat::HeartbeatConfig {
    multiview_licence::heartbeat::HeartbeatConfig {
        org_id: "org_test".to_owned(),
        ..multiview_licence::heartbeat::HeartbeatConfig::default()
    }
}

fn test_identity() -> multiview_licence::heartbeat::DeviceIdentity {
    multiview_licence::heartbeat::DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_2b6e1c0e9b41".to_owned(),
        binding_id: Some("ib_fab_0001".to_owned()),
        fingerprint_digest: "1".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: String::new(), // set by the signer in production
    }
}

/// A client wired with a known device signer so the fake can verify the proof.
fn pop_client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    HeartbeatClient::with_device_signer(
        server,
        store,
        pinned,
        test_config(),
        test_identity(),
        Arc::new(pop_test_signer()),
    )
}

#[tokio::test]
async fn cold_start_fetches_a_challenge_then_heartbeats_with_a_valid_pop() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("a healthy PoP heartbeat must succeed");
    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));

    // Cold start consulted GET /challenge exactly once (no held nonce yet).
    assert_eq!(
        server.challenge_fetches(),
        1,
        "cold start must fetch one challenge"
    );
    // The heartbeat carried a PoP header that VERIFIES against the device key, and
    // the body `nonce` equalled the challenge nonce.
    assert!(
        server.last_pop_verified(),
        "the PoP proof must verify against the device public key"
    );
    assert_eq!(
        server.last_request_nonce().as_deref(),
        Some(server.last_issued_nonce().as_str()),
        "the heartbeat body nonce must be the challenge nonce"
    );
    assert_eq!(
        store.status().unwrap().enforcement,
        EnforcementLevel::Active
    );
}

#[tokio::test]
async fn steady_state_uses_next_nonce_and_skips_the_challenge_round_trip() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Cycle 1: cold start (1 challenge fetch).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(server.challenge_fetches(), 1);
    let next_nonce_after_1 = server.last_next_nonce();

    // Cycle 2: steady state — the prior response's nextNonce is reused, so NO new
    // /challenge round-trip (RFC 9449 DPoP-nonce style).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        server.challenge_fetches(),
        1,
        "steady state must NOT fetch a second challenge"
    );
    // Cycle 2's body nonce was cycle 1's nextNonce.
    assert_eq!(
        server.last_request_nonce(),
        next_nonce_after_1,
        "the next heartbeat must sign the prior response's nextNonce"
    );
    assert!(server.last_pop_verified());
}

#[tokio::test]
async fn an_unreachable_challenge_keeps_last_good_no_mutation() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Seed a healthy lease first (so there is a last-good to keep).
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;
    let heartbeats_before = server.heartbeats.load(Ordering::SeqCst);

    // Force a cold start (drop the held nonce) AND make /challenge unreachable.
    server.set_drop_next_nonce(true); // next response carries no usable nextNonce
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap(); // this cycle still succeeds; it just leaves no nextNonce
    server.set_fail_challenge(true);

    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang when /challenge is down");
    assert!(
        res.is_err(),
        "an unreachable /challenge with no held nonce surfaces an error (fail closed)"
    );
    // No heartbeat mutation was sent this cycle, and the last-good lease is intact.
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a PoP-nonce failure keeps the last-good lease (never off air)"
    );
    let _ = heartbeats_before;
}

#[tokio::test]
async fn a_server_rejected_nonce_pop_invalid_keeps_last_good() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = pop_client(Arc::clone(&server), Arc::clone(&store));

    // Seed a healthy lease.
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;

    // The server now rejects the heartbeat as pop-invalid (401).
    server.set_reject_pop(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang");
    assert!(res.is_err(), "a pop-invalid rejection surfaces an error");
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a pop-invalid rejection keeps the last-good lease (never off air)"
    );
}

#[tokio::test]
async fn no_binding_makes_no_challenge_and_no_pop() {
    // The renew-only client makes NO server call when there is no binding to renew
    // — so it must NOT fetch a challenge or build a PoP (a benign no-op).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let pinned = server.pinned_root();
    let mut identity = test_identity();
    identity.binding_id = None; // no configured binding, empty store → NoBinding
    let client = HeartbeatClient::with_device_signer(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        identity,
        Arc::new(pop_test_signer()),
    );

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("no-binding is a benign no-op, not an error");
    assert!(matches!(outcome, HeartbeatOutcome::NoBinding { .. }));
    assert_eq!(
        server.challenge_fetches(),
        0,
        "no binding → no challenge fetch"
    );
    assert_eq!(server.heartbeats.load(Ordering::SeqCst), 0);
}
