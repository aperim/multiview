//! Local lease-store + install-path tests (CONSPECT-1): a verified lease
//! installs and the store computes the ladder state at an injected `now`; a
//! tampered lease is rejected `SignatureInvalid`; a stale (older) lease is
//! rejected `Stale`; a below-threshold fingerprint is rejected
//! `FingerprintMismatch`. The store is thread-safe, in-memory, best-effort, and
//! never reads a system clock itself (the clock is injected).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signer, SigningKey};
use multiview_licence::entitlement::{
    Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier,
};
use multiview_licence::lease::{Lease, LeaseSource};
use multiview_licence::status::EnforcementLevel;
use multiview_licence::store::{InstallError, LeaseBinding, LeaseStore};
use multiview_licence::verify::{PinnedKey, SignedLease};
use multiview_licence::{compute_ladder_state, LadderState, ACTIVATION_WINDOW_DAYS};
use rand_core::OsRng;

fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).unwrap()
}

fn keypair() -> (SigningKey, PinnedKey) {
    let mut rng = OsRng;
    let key = SigningKey::generate(&mut rng);
    let pinned = PinnedKey::from_verifying_key(&key.verifying_key());
    (key, pinned)
}

fn lease_at(serial: &str, granted: DateTime<Utc>) -> Lease {
    Lease::new_full(
        serial.to_owned(),
        granted,
        LeaseSource::Online,
        ACTIVATION_WINDOW_DAYS,
    )
}

fn sign(key: &SigningKey, lease: &Lease) -> SignedLease {
    let msg = SignedLease::signing_bytes(lease);
    let sig = key.sign(&msg);
    SignedLease::new(lease.clone(), sig.to_bytes())
}

/// A binding carrying a strong fingerprint score (same machine) over a signed
/// lease, the standard install unit the three install paths converge on.
fn binding(key: &SigningKey, lease: &Lease, score: u8) -> LeaseBinding {
    LeaseBinding::new(
        sign(key, lease),
        Entitlement::new(
            Tier::new("studio".to_owned()),
            HardwareClass::Standard,
            HardwareClass::Standard,
            GpuLimit::Limited(2),
            lease.clone(),
            EntitlementFlags::default(),
        ),
        score,
        None,
    )
}

#[test]
fn a_verified_lease_installs_and_the_store_computes_the_ladder_state() {
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-LIVE01", now);
    let store = LeaseStore::with_clock(Arc::new(move || now));

    let installed = store
        .install_binding(&binding(&key, &lease, 100), &pinned, now)
        .expect("a valid binding installs");
    assert_eq!(installed.serial, "serial-LIVE01");

    // The store now reports a status computed via the crate's ladder.
    let status = store.status().expect("a status after install");
    assert_eq!(status.tier, "studio");
    assert_eq!(status.state, LadderState::Compliant);
    assert_eq!(status.enforcement, EnforcementLevel::Active);
    assert_eq!(status.hardware_class.licensed, HardwareClass::Standard);
    assert_eq!(status.hardware_class.detected, HardwareClass::Standard);
    assert_eq!(status.gpu_limit, GpuLimit::Limited(2));
    assert_eq!(status.lease.serial, "serial-LIVE01");
    assert_eq!(status.lease.source, LeaseSource::Online);
    // The view's computed state matches the crate's compute_ladder_state directly.
    let outcome = compute_ladder_state(&store.ladder_input(now).expect("input"));
    assert_eq!(status.state, outcome.state);
}

#[test]
fn the_store_recomputes_state_as_the_clock_advances() {
    // Same installed lease, two different `now`s: compliant at grant, grace once
    // a day past expiry. The store recomputes from the injected clock each read.
    let (key, pinned) = keypair();
    let granted = epoch();
    let lease = lease_at("serial-AGE", granted);
    let store = LeaseStore::new();
    store
        .install_binding(&binding(&key, &lease, 100), &pinned, granted)
        .expect("install");

    // Compliant within term.
    assert_eq!(
        store.status_at(granted).expect("status").state,
        LadderState::Compliant
    );
    // One day past the 35-day term → grace.
    let in_grace = lease.expires_at + Duration::days(1);
    assert_eq!(
        store.status_at(in_grace).expect("status").state,
        LadderState::Grace
    );
}

#[test]
fn a_tampered_lease_is_rejected_signature_invalid() {
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-OK", now);
    let mut b = binding(&key, &lease, 100);
    // Mutate the covered serial AFTER signing — the signature no longer matches.
    b.signed.lease.serial = "serial-EVIL".to_owned();

    let err = store_install_err(&b, &pinned, now);
    assert!(
        matches!(err, InstallError::SignatureInvalid),
        "tampered lease must be SignatureInvalid, got {err:?}"
    );
}

#[test]
fn a_below_threshold_fingerprint_is_rejected_fingerprint_mismatch() {
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-DRIFT", now);
    // Score 69 is one below FINGERPRINT_MATCH_THRESHOLD (70): a *new* machine.
    let b = binding(&key, &lease, 69);

    let err = store_install_err(&b, &pinned, now);
    assert!(
        matches!(err, InstallError::FingerprintMismatch { score: 69, .. }),
        "below-threshold fingerprint must be FingerprintMismatch, got {err:?}"
    );
}

#[test]
fn a_threshold_fingerprint_installs() {
    // Exactly the threshold (70) is the SAME machine (drift tolerated) — installs.
    let (key, pinned) = keypair();
    let now = epoch();
    let lease = lease_at("serial-EDGE", now);
    let store = LeaseStore::new();
    assert!(store
        .install_binding(&binding(&key, &lease, 70), &pinned, now)
        .is_ok());
}

#[test]
fn an_older_lease_is_rejected_stale_after_a_newer_one_installs() {
    let (key, pinned) = keypair();
    let new_granted = epoch();
    let old_granted = epoch() - Duration::days(10);
    let store = LeaseStore::new();

    // Install the newer grant first.
    store
        .install_binding(
            &binding(&key, &lease_at("serial-NEW", new_granted), 100),
            &pinned,
            new_granted,
        )
        .expect("newer installs");

    // A subsequently-presented OLDER grant must be rejected as stale (replay /
    // rollback protection): the active lease never goes backwards.
    let err = store
        .install_binding(
            &binding(&key, &lease_at("serial-OLD", old_granted), 100),
            &pinned,
            new_granted,
        )
        .expect_err("an older grant must be rejected");
    assert!(
        matches!(err, InstallError::Stale { .. }),
        "an older grant must be Stale, got {err:?}"
    );
    // The active lease is unchanged (still the newer one).
    assert_eq!(store.status().expect("status").lease.serial, "serial-NEW");
}

/// Drive an install against a fresh store and return the error it must produce.
fn store_install_err(b: &LeaseBinding, pinned: &PinnedKey, now: DateTime<Utc>) -> InstallError {
    let store = LeaseStore::new();
    store
        .install_binding(b, pinned, now)
        .expect_err("install must fail")
}

#[test]
fn the_store_retains_the_verified_fingerprint_score_for_support_context() {
    // The support-ticket context auto-attaches the fingerprint *score* (a number,
    // never a raw identifier — brief §8). The store retains the score the install
    // verified, and reports `None` before any install.
    let (key, pinned) = keypair();
    let now = epoch();
    let store = LeaseStore::with_clock(Arc::new(move || now));
    assert_eq!(
        store.fingerprint_score(),
        None,
        "no lease installed yet → no score (never a false zero)"
    );

    let lease = lease_at("serial-FP", now);
    store
        .install_binding(&binding(&key, &lease, 88), &pinned, now)
        .expect("a valid binding installs");
    assert_eq!(
        store.fingerprint_score(),
        Some(88),
        "the store retains the exact verified score the install accepted"
    );
}

#[test]
fn the_store_records_and_reports_the_instance_binding_id() {
    // The heartbeat client records the server-issued instanceBindingId on a
    // genuine install so the device's instance identity is durable (the
    // cross-instance-replay guard reads it back). `None` until one is recorded.
    let now = epoch();
    let store = LeaseStore::with_clock(Arc::new(move || now));
    assert_eq!(
        store.current_binding_id(),
        None,
        "no binding recorded yet → None (the device is genuinely fresh)"
    );
    store.record_binding_id("ib_device_0001");
    assert_eq!(
        store.current_binding_id(),
        Some("ib_device_0001".to_owned()),
        "the store reports the recorded instance binding id"
    );
    // A later genuine install of a different binding (same device, re-bound)
    // overwrites it — the store is a single-value cache of the current identity.
    store.record_binding_id("ib_device_0002");
    assert_eq!(
        store.current_binding_id(),
        Some("ib_device_0002".to_owned()),
        "recording overwrites — the store holds the current binding identity"
    );
}
