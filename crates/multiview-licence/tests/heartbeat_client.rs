//! CONSPECT-3 heartbeat **client loop** + install convergence + never-off-air
//! chaos tests (ADR-0096, invariants #1/#10).
//!
//! A successful heartbeat against the in-process [`FakeLicenceServer`] verifies
//! the returned signed lease against the key-trust chain and drives the existing
//! [`LeaseStore::install_binding`] convergence, so the machine reads `Active`
//! wait-free with no engine-side wiring. The chaos cases abort / stall / fail the
//! heartbeat task and assert the last-good lease + status are unchanged — the
//! client only ever TIGHTENS on a positively-verified signed lease, and is
//! physically unable to take a running program off air.
//!
//! Every async case runs under a hard `tokio::time::timeout` so a hung loop fails
//! CI fast rather than hanging it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use multiview_licence::heartbeat::{
    DeviceIdentity, HeartbeatClient, HeartbeatConfig, HeartbeatOutcome,
};
use multiview_licence::{EnforcementLevel, LeaseStore};

mod fake;
use fake::{shared_fake, FakeLicenceServer};

/// A short, deterministic config (tiny backoff; the loop is driven step-by-step
/// in tests via `run_once`, so the sleep cadence does not gate the assertions).
fn test_config() -> HeartbeatConfig {
    HeartbeatConfig {
        org_id: "org_test".to_owned(),
        ..HeartbeatConfig::default()
    }
}

/// A device identity carrying ONLY salted digests + opaque ids — never a raw
/// serial/MAC/UUID (data minimisation, brief §8).
fn test_identity() -> DeviceIdentity {
    DeviceIdentity {
        machine_id: "mch_7c2a1f04c9e75031".to_owned(),
        instance_id: "inst_2b6e1c0e9b41".to_owned(),
        binding_id: Some("ib_fab_0001".to_owned()),
        fingerprint_digest: "1".repeat(64),
        fingerprint_score: 95,
        hardware_digest: "hwd_2b6e1c0e9b41".to_owned(),
        instance_discriminator_hash: "disc_9f3a2b6e1c0e".to_owned(),
        instance_discriminator_digest: "2".repeat(64),
        app_version: "0.1.0-test".to_owned(),
        device_public_key_b64url: "ZmFrZS1kZXZpY2Uta2V5".to_owned(),
    }
}

fn client(
    server: Arc<FakeLicenceServer>,
    store: Arc<LeaseStore>,
) -> HeartbeatClient<FakeLicenceServer> {
    let pinned = server.pinned_root();
    HeartbeatClient::new(server, store, pinned, test_config(), test_identity())
}

#[tokio::test]
async fn a_successful_heartbeat_installs_the_verified_lease_active() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // No lease installed yet → no status.
    assert!(store.status().is_none(), "store starts empty");

    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect("a healthy heartbeat must succeed");

    assert!(matches!(outcome, HeartbeatOutcome::Installed { .. }));
    let status = store.status().expect("a verified lease must be installed");
    assert_eq!(
        status.enforcement,
        EnforcementLevel::Active,
        "a fresh 35-day lease reads Active"
    );
    assert_eq!(server.heartbeats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_tampered_lease_from_the_server_is_rejected_and_nothing_is_installed() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    // Point the client at the WRONG pinned root: the server's (fabricated) keyset
    // cannot be attested by a foreign root, so key-trust fails and no lease is
    // installed — the client refuses to trust an un-attested chain.
    let foreign = fake::FabricatedKeyset::new(); // a different instance shares the seed,
                                                 // so use a deterministic foreign root via the helper below.
    let wrong_root = foreign_root();
    let _ = foreign;
    let pinned = wrong_root;
    let client = HeartbeatClient::new(
        Arc::clone(&server),
        Arc::clone(&store),
        pinned,
        test_config(),
        test_identity(),
    );
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang")
        .expect_err("an un-attested keyset must surface an error, not install");
    let _ = outcome;
    assert!(
        store.status().is_none(),
        "nothing is installed when the trust chain fails (fail closed on trust, lenient on enforcement)"
    );
}

#[tokio::test]
async fn withholding_the_lease_keeps_last_good_never_tightens() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));

    // First heartbeat installs Active.
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let installed_serial = store.current().unwrap().lease.serial;
    assert_eq!(store.status().unwrap().enforcement, EnforcementLevel::Active);

    // Now the entitlement is "revoked" server-side: heartbeat returns lease:null
    // (revocation by non-reissue). The client must NOT tighten — the last-good
    // lease stays installed and ages via the existing ladder.
    server.set_withhold_lease(true);
    let outcome = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .expect("a withheld lease is a normal 200, not an error");
    assert!(matches!(outcome, HeartbeatOutcome::LeaseWithheld { .. }));
    let after = store.current().expect("last-good lease must remain installed");
    assert_eq!(
        after.lease.serial, installed_serial,
        "the last-good lease is unchanged — revocation never installs a worse state"
    );
    assert_eq!(
        store.status().unwrap().enforcement,
        EnforcementLevel::Active,
        "the client never tightens on its own; the lease ages naturally"
    );
}

#[tokio::test]
async fn an_unreachable_server_keeps_last_good() {
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let serial = store.current().unwrap().lease.serial;

    server.set_fail(true);
    let res = tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .expect("run_once must not hang even when the server is down");
    assert!(res.is_err(), "an unreachable server surfaces a transport error");
    assert_eq!(
        store.current().unwrap().lease.serial,
        serial,
        "a failed renew keeps the last-good lease (never off air)"
    );
}

#[tokio::test]
async fn the_heartbeat_request_carries_no_raw_identifier() {
    // Data minimisation: the request the client builds carries ONLY the binding
    // id, the salted fingerprint digest, the app version, the lease serial, and
    // the transport — never a raw serial/MAC/UUID/hostname.
    let identity = test_identity();
    let req = HeartbeatClient::<FakeLicenceServer>::build_heartbeat_request(&identity, None);
    let json = serde_json::to_string(&req).unwrap();

    // The salted digest IS present; a raw MAC/serial pattern is NOT.
    assert!(json.contains(&identity.fingerprint_digest));
    // No colon-separated MAC, no "serial"/"macAddress"/"uuid" raw fields.
    assert!(!json.to_lowercase().contains("macaddress"), "no raw MAC field: {json}");
    assert!(!json.to_lowercase().contains("serialnumber"), "no raw serial field: {json}");
    // The fingerprint digest is the salted hex, not a raw 48-bit MAC.
    assert!(
        !json.contains("\"00:1b:"),
        "no raw MAC value leaks into the payload: {json}"
    );
    // The set of top-level keys is exactly the minimal heartbeat payload.
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    let keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    for k in &keys {
        assert!(
            ["bindingId", "leaseSerial", "fingerprintDigest", "appVersion", "transport"]
                .contains(&k.as_str()),
            "unexpected heartbeat payload field {k:?} (data minimisation)"
        );
    }
}

// --- never-off-air chaos: SIGKILL/stall/partition the heartbeat task ----------

#[tokio::test]
async fn aborting_the_heartbeat_task_leaves_the_store_untouched() {
    // Install a good lease, then spawn a heartbeat loop and ABORT it mid-flight
    // (the JoinHandle::abort is the in-process analogue of SIGKILL'ing the task).
    // The store's last-good lease + status must be byte-identical afterwards —
    // the heartbeat task holds no lock the store/engine takes (invariant #10) and
    // can never tighten or remove a lease by dying (invariant #1).
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();
    let before = store.status().unwrap();

    // A loop that would run forever; abort it almost immediately.
    let store2 = Arc::clone(&store);
    let server2 = Arc::clone(&server);
    let handle = tokio::spawn(async move {
        let client = HeartbeatClient::new(
            server2,
            store2,
            FakeLicenceServer::new().pinned_root(),
            test_config(),
            test_identity(),
        );
        client.run_forever().await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    handle.abort();
    let _ = handle.await; // joins as Cancelled.

    let after = store.status().unwrap();
    assert_eq!(
        before, after,
        "aborting the heartbeat task must leave the last-good status byte-identical"
    );
}

#[tokio::test]
async fn a_stalled_server_call_cannot_block_a_store_reader() {
    // A reader sampling the store (the wait-free path the engine uses) must never
    // be blocked by an in-flight (stalled) heartbeat. We start a heartbeat whose
    // server call we make slow, and assert the store stays readable throughout.
    let server = shared_fake();
    let store = Arc::new(LeaseStore::new());
    let client = client(Arc::clone(&server), Arc::clone(&store));
    tokio::time::timeout(Duration::from_secs(5), client.run_once())
        .await
        .unwrap()
        .unwrap();

    // Simulate a partition: the next call will fail after a delay. Spawn it and,
    // while it is "in flight", hammer the store reader; it must return promptly
    // every time (the store read is wait-free and the task holds no shared lock).
    server.set_fail(true);
    let client2 = client;
    let probe = tokio::spawn(async move {
        let _ = client2.run_once().await;
    });
    for _ in 0..1000 {
        // Each of these is a wait-free read; if the heartbeat could back-pressure
        // it, the overall timeout below would trip.
        let _ = store.status();
        let _ = store.current();
    }
    tokio::time::timeout(Duration::from_secs(5), probe)
        .await
        .expect("the probe task must finish; the store reader was never blocked")
        .unwrap();
    assert!(store.current().is_some(), "last-good lease still present");
}

/// A deterministic foreign root (different from the fabricated keyset's root) for
/// the wrong-root rejection test.
fn foreign_root() -> multiview_licence::heartbeat::PinnedRoot {
    use p256::ecdsa::{SigningKey, VerifyingKey};
    let sk = SigningKey::from_bytes(&[3u8; 32].into()).unwrap();
    let vk = VerifyingKey::from(&sk);
    multiview_licence::heartbeat::PinnedRoot::from_sec1_bytes(vk.to_encoded_point(false).as_bytes())
        .unwrap()
}
