//! Shared test scaffolding for the CONSPECT-3 heartbeat tests: a fabricated,
//! root-attested keyset (whose root + intermediate private keys we control, so
//! we can mint a valid signed lease and exercise the revocation contract) and an
//! in-process [`FakeLicenceServer`] that returns locally-signed leases.
//!
//! This is `tests/fake/mod.rs` (a submodule, not a top-level `tests/*.rs`) so it
//! is shared by the integration tests without being compiled as its own test
//! binary.
// A shared test-helper module included by multiple integration-test crates; not
// every crate exercises every helper, so `dead_code` / `unreachable_pub` are
// expected and silenced here (test-only scaffolding, never product code).
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::doc_markdown,
    clippy::needless_pass_by_value,
    clippy::unused_self,
    clippy::missing_panics_doc
)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey as EdKey};
// `p256::ecdsa::SigningKey::sign` is provided by the inherent `Signer` impl that
// `ecdsa` brings into scope through the type itself (no separate trait import is
// needed in p256 0.13); `Signature` is `P256Sig`.
use p256::ecdsa::{Signature as P256Sig, SigningKey as P256Key};

use multiview_licence::heartbeat::{
    canonical_key_preimage, canonical_revocation_preimage, ActivateRequest, ActivateResponse,
    EnforcementState, HeartbeatError, HeartbeatRequest, HeartbeatResponse, LeaseBodyFields,
    LicenceServer, LicensingKeys, PinnedRoot, ServerLease, TrustedKeys,
};

/// The live production root verifying key (ECDSA P-256, base64url uncompressed),
/// re-exported for the key-trust tests.
pub const ROOT_PUB_B64URL: &str =
    "BN4f6BIHOFZmFqXp9YM1U65bJTGpOOob1I9X8C_FpfJOWanTCs4Z3c-l8C1wqH4g8Rl01VNkQNC78XixViLiwRY";

/// Decode a base64url (no-pad) string, the well-known encoding for key material.
pub fn b64url(s: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .unwrap()
}

const FAB_KID: &str = "intermediate-fab";
const FAB_LICENCE: &str = "lic_fab_0001";
const FAB_BINDING: &str = "ib_fab_0001";
const FAB_NOW_MS: i64 = 1_790_000_000_000;
const FAB_VALID_FROM: i64 = 1_781_510_167_548;
const FAB_VALID_UNTIL: i64 = 1_850_630_167_548;
const FAB_ISSUED_AT: i64 = 1_781_570_867_301;

/// A fabricated keyset rooted at a P-256 key we control.
pub struct FabricatedKeyset {
    root: P256Key,
    intermediate: EdKey,
}

impl FabricatedKeyset {
    pub fn new() -> Self {
        Self {
            root: P256Key::from_bytes(&[7u8; 32].into()).expect("test root key"),
            intermediate: EdKey::from_bytes(&[9u8; 32]),
        }
    }

    pub fn now_ms(&self) -> i64 {
        FAB_NOW_MS
    }
    pub fn kid(&self) -> &'static str {
        FAB_KID
    }
    pub fn licence_id(&self) -> &'static str {
        FAB_LICENCE
    }

    pub fn pinned_root(&self) -> PinnedRoot {
        let vk = p256::ecdsa::VerifyingKey::from(&self.root);
        PinnedRoot::from_sec1_bytes(vk.to_encoded_point(false).as_bytes()).expect("fab root parse")
    }

    fn root_sign(&self, msg: &[u8]) -> String {
        let sig: P256Sig = self.root.sign(msg);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes())
    }

    fn intermediate_pub_b64url(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(self.intermediate.verifying_key().to_bytes())
    }

    pub fn keys(&self) -> LicensingKeys {
        self.keys_with_revocation(&[])
    }

    /// Build a well-formed, root-attested well-known doc with `revoked` listed in
    /// the (root-signed) revocation set.
    pub fn keys_with_revocation(&self, revoked: &[&str]) -> LicensingKeys {
        self.keys_full(revoked, "lease", "current", FAB_VALID_FROM, FAB_VALID_UNTIL)
    }

    /// Build a root-attested well-known doc whose sole `lease_keys` entry carries
    /// the given `key_type` + `status` — its `root_sig` is computed over the
    /// pre-image that INCLUDES that `key_type`, so it is genuinely root-attested
    /// (the attack the verifier must still reject: a root-attested NON-lease key
    /// presented as a lease signer). `status` is the UNSIGNED operational hint.
    pub fn keys_with_signer(&self, key_type: &str, status: &str) -> LicensingKeys {
        self.keys_full(&[], key_type, status, FAB_VALID_FROM, FAB_VALID_UNTIL)
    }

    /// Like [`keys_with_signer`] but with an explicit (signed) validity window —
    /// to prove that forging the UNSIGNED `status` to "current" does NOT rescue a
    /// key whose SIGNED validity window excludes `now`.
    pub fn keys_with_signer_validity(
        &self,
        key_type: &str,
        status: &str,
        valid_from: i64,
        valid_until: i64,
    ) -> LicensingKeys {
        self.keys_full(&[], key_type, status, valid_from, valid_until)
    }

    /// A keyset whose sole lease key has the given `status` but is named in the
    /// (root-signed) revocation list — to prove the SIGNED revocation list still
    /// drops it even when `status` is forged to "current".
    pub fn keys_with_signer_revoked(&self, status: &str) -> LicensingKeys {
        self.keys_full(&[FAB_KID], "lease", status, FAB_VALID_FROM, FAB_VALID_UNTIL)
    }

    /// Build a keyset whose intermediate carries a NEGATIVE `valid_from`. The
    /// root_sig is computed over the pre-image with that negative value, so the
    /// entry is structurally root-attested — the rejection must come from the
    /// negative-timestamp gate, not a signature mismatch.
    pub fn keys_with_negative_valid_from(&self) -> LicensingKeys {
        let pubkey = self.intermediate.verifying_key().to_bytes().to_vec();
        let pre = canonical_key_preimage(
            FAB_KID,
            "lease",
            "conspect.key-attestation.v1",
            &pubkey,
            -1,
            FAB_VALID_UNTIL,
        );
        let root_sig = self.root_sign(&pre);
        let rev_pre =
            canonical_revocation_preimage(FAB_ISSUED_AT, "conspect.key-revocation.v1", &[]);
        let rev_sig = self.root_sign(&rev_pre);
        let root_pub = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            p256::ecdsa::VerifyingKey::from(&self.root)
                .to_encoded_point(false)
                .as_bytes(),
        );
        let json = serde_json::json!({
            "version": 1,
            "root": { "kid": "root", "algorithm": "ecdsa-p256-sha256", "public_key": root_pub,
                "public_key_encoding": "base64url-uncompressed-p256-point" },
            "attestation_contract": {
                "key_statement": "conspect.key-attestation.v1",
                "revocation_statement": "conspect.key-revocation.v1",
                "encoding": "deterministic-cbor-rfc8949-section-4.2.1",
                "key_pre_image": ["key_id","key_type","statement","public_key","valid_from","valid_until"],
                "revocation_pre_image": ["issued_at","statement","revoked_key_ids"],
                "field_order": "canonical", "signature": "ecdsa-p256-sha256-raw-r-s-base64url",
                "public_key_encoding": "raw-32-byte-ed25519-point", "time_unit": "epoch-milliseconds" },
            "lease_keys": [{ "kid": FAB_KID, "key_type": "lease", "algorithm": "ed25519",
                "public_key": self.intermediate_pub_b64url(), "valid_from": -1,
                "valid_until": FAB_VALID_UNTIL, "status": "current", "root_sig": root_sig }],
            "update_keys": [],
            "revocation": { "statement": "conspect.key-revocation.v1", "issued_at": FAB_ISSUED_AT,
                "revoked_key_ids": [], "root_revocation_sig": rev_sig }
        });
        serde_json::from_value(json).expect("fabricated keyset must parse")
    }

    fn keys_full(
        &self,
        revoked: &[&str],
        key_type: &str,
        status: &str,
        valid_from: i64,
        valid_until: i64,
    ) -> LicensingKeys {
        let pubkey = self.intermediate.verifying_key().to_bytes().to_vec();
        // The root_sig covers the pre-image that includes key_type + the validity
        // window, so the entry is genuinely root-attested for THIS key_type and
        // window (mirrors how Conspect signs every intermediate). `status` is NOT
        // in the pre-image — it is the unsigned, forgeable operational hint.
        let pre = canonical_key_preimage(
            FAB_KID,
            key_type,
            "conspect.key-attestation.v1",
            &pubkey,
            valid_from,
            valid_until,
        );
        let root_sig = self.root_sign(&pre);
        let revoked_owned: Vec<String> = revoked.iter().map(|s| (*s).to_owned()).collect();
        let rev_pre = canonical_revocation_preimage(
            FAB_ISSUED_AT,
            "conspect.key-revocation.v1",
            &revoked_owned,
        );
        let rev_sig = self.root_sign(&rev_pre);
        let root_pub = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            p256::ecdsa::VerifyingKey::from(&self.root)
                .to_encoded_point(false)
                .as_bytes(),
        );

        let json = serde_json::json!({
            "version": 1,
            "root": {
                "kid": "root",
                "algorithm": "ecdsa-p256-sha256",
                "public_key": root_pub,
                "public_key_encoding": "base64url-uncompressed-p256-point"
            },
            "attestation_contract": {
                "key_statement": "conspect.key-attestation.v1",
                "revocation_statement": "conspect.key-revocation.v1",
                "encoding": "deterministic-cbor-rfc8949-section-4.2.1",
                "key_pre_image": ["key_id","key_type","statement","public_key","valid_from","valid_until"],
                "revocation_pre_image": ["issued_at","statement","revoked_key_ids"],
                "field_order": "canonical",
                "signature": "ecdsa-p256-sha256-raw-r-s-base64url",
                "public_key_encoding": "raw-32-byte-ed25519-point",
                "time_unit": "epoch-milliseconds"
            },
            "lease_keys": [{
                "kid": FAB_KID,
                "key_type": key_type,
                "algorithm": "ed25519",
                "public_key": self.intermediate_pub_b64url(),
                "valid_from": valid_from,
                "valid_until": valid_until,
                "status": status,
                "root_sig": root_sig
            }],
            "update_keys": [],
            "revocation": {
                "statement": "conspect.key-revocation.v1",
                "issued_at": FAB_ISSUED_AT,
                "revoked_key_ids": revoked_owned,
                "root_revocation_sig": rev_sig
            }
        });
        serde_json::from_value(json).expect("fabricated keyset must parse")
    }

    pub fn trusted(&self) -> TrustedKeys {
        TrustedKeys::verify(&self.keys(), &self.pinned_root(), self.now_ms())
            .expect("the fabricated keyset must verify against its own root")
    }

    /// The default fabricated lease serial (a UUIDv7).
    pub fn lease_serial(&self) -> &'static str {
        "01931d4e-7b2a-7c00-9f3a-2b6e1c0e9b41"
    }
    pub fn binding_id(&self) -> &'static str {
        FAB_BINDING
    }

    /// Mint a signed lease (bare Ed25519 over a canonical-CBOR body), `term_days`
    /// out from `now_ms`.
    pub fn sign_lease(&self, term_days: i64) -> ServerLease {
        self.sign_lease_expiring_at(self.now_ms() + term_days * 86_400_000)
    }

    /// Mint a signed lease whose signed `not_after` is exactly `not_after_ms` —
    /// the cryptographically-signed expiry the installer MUST honour (so a past
    /// `not_after` exercises the expiry/replay rejection).
    pub fn sign_lease_expiring_at(&self, not_after_ms: i64) -> ServerLease {
        self.sign_lease_for(FAB_BINDING, not_after_ms)
    }

    /// A foreign instance binding id — a DIFFERENT device's binding, for the
    /// cross-instance replay test.
    pub fn foreign_binding_id(&self) -> &'static str {
        "ib_other_device_9999"
    }

    /// Mint a correctly-signed lease whose body binds `binding_id` and expires at
    /// `not_after_ms`. Lets a test mint a lease for ANOTHER device (foreign
    /// binding) and/or with an absolute-past expiry — both still cryptographically
    /// valid, so the rejection must come from the install-time identity/expiry
    /// gates, not the signature.
    pub fn sign_lease_for(&self, binding_id: &str, not_after_ms: i64) -> ServerLease {
        self.sign_body(LeaseBodyFields {
            licence_id: FAB_LICENCE.to_owned(),
            instance_binding_id: binding_id.to_owned(),
            serial: self.lease_serial().to_owned(),
            not_after: not_after_ms,
            gpu_limit: Some(2),
            hardware_class: Some("standard".to_owned()),
        })
    }

    /// Mint a signed lease with a DISTINCT (older) serial expiring at
    /// `not_after_ms` — the replay-attack lease (a different serial than the
    /// current one, so installing it is not a serial-collision no-op).
    pub fn sign_old_lease(&self, not_after_ms: i64) -> ServerLease {
        self.sign_body(LeaseBodyFields {
            licence_id: FAB_LICENCE.to_owned(),
            instance_binding_id: FAB_BINDING.to_owned(),
            serial: "01930000-0000-7000-8000-000000000001".to_owned(),
            not_after: not_after_ms,
            gpu_limit: Some(2),
            hardware_class: Some("standard".to_owned()),
        })
    }

    /// Sign an arbitrary lease body — exactly the bytes the verifier re-checks.
    /// The signed `not_after` mirror field on the envelope matches the body.
    pub fn sign_body(&self, body: LeaseBodyFields) -> ServerLease {
        let lease_bytes = body.to_canonical_cbor();
        let sig = self.intermediate.sign(&lease_bytes);
        ServerLease {
            serial: body.serial.clone(),
            licence_id: Some(body.licence_id.clone()),
            instance_binding_id: Some(body.instance_binding_id.clone()),
            not_after: body.not_after,
            signature: hex::encode(sig.to_bytes()),
            signer_key_id: FAB_KID.to_owned(),
            lease_bytes: base64::engine::general_purpose::STANDARD.encode(&lease_bytes),
        }
    }

    /// Sign a canonical-CBOR lease body that genuinely OMITS `omit` (a required
    /// field) — to prove the verifier fails closed on a missing field rather than
    /// defaulting it to empty. Builds the map by hand (canonical key order).
    ///
    /// The `licence_id` value is padded so the total CBOR length is a multiple of
    /// 3 (its standard-base64 has NO '=' padding) — this ISOLATES the missing-field
    /// rejection from the base64-padding decode path, so the test proves blocker #4
    /// independent of blocker #3.
    pub fn sign_body_omitting(&self, omit: &str) -> ServerLease {
        let not_after = self.now_ms() + 35 * 86_400_000;
        // Search a licence_id length that makes the encoded body len % 3 == 0.
        for pad in 0..3 {
            let licence_id = format!("{FAB_LICENCE}{}", "x".repeat(pad));
            let mut pairs: Vec<(&str, CborScalar)> = vec![
                (
                    "instance_binding_id",
                    CborScalar::Text(FAB_BINDING.to_owned()),
                ),
                ("licence_id", CborScalar::Text(licence_id.clone())),
                ("not_after", CborScalar::Uint(not_after)),
                ("serial", CborScalar::Text(self.lease_serial().to_owned())),
            ];
            pairs.retain(|(k, _)| *k != omit);
            pairs.sort_by(|a, b| {
                a.0.len()
                    .cmp(&b.0.len())
                    .then(a.0.as_bytes().cmp(b.0.as_bytes()))
            });
            let mut bytes = Vec::new();
            cbor_map_head(&mut bytes, pairs.len());
            for (k, v) in &pairs {
                cbor_text(&mut bytes, k);
                match v {
                    CborScalar::Text(s) => cbor_text(&mut bytes, s),
                    CborScalar::Uint(n) => cbor_uint(&mut bytes, *n),
                }
            }
            if bytes.len() % 3 != 0 {
                continue; // would base64-pad; try a different licence_id length
            }
            let sig = self.intermediate.sign(&bytes);
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            assert!(!encoded.ends_with('='), "chosen body must be unpadded");
            return ServerLease {
                serial: self.lease_serial().to_owned(),
                licence_id: Some(licence_id),
                instance_binding_id: Some(FAB_BINDING.to_owned()),
                not_after,
                signature: hex::encode(sig.to_bytes()),
                signer_key_id: FAB_KID.to_owned(),
                lease_bytes: encoded,
            };
        }
        panic!("could not build an unpadded omitting body for {omit}");
    }

    /// Sign a body whose `gpu_limit` is the RAW signed integer `raw` (which may be
    /// negative, or larger than `u32::MAX`) — so the verifier's gpu_limit handling
    /// is exercised with a present-but-out-of-range value. All required fields are
    /// present + valid; only gpu_limit is hostile. The CBOR integer is emitted
    /// canonically (major 1 for negatives, the 8-byte form for oversized).
    pub fn sign_body_with_raw_gpu_limit(&self, raw: i64) -> ServerLease {
        let not_after = self.now_ms() + 35 * 86_400_000;
        // Canonical key order: gpu_limit, instance_binding_id, licence_id,
        // not_after, serial (by length then bytewise).
        let mut bytes = Vec::new();
        cbor_map_head(&mut bytes, 5);
        cbor_text(&mut bytes, "gpu_limit");
        cbor_int(&mut bytes, raw);
        cbor_text(&mut bytes, "instance_binding_id");
        cbor_text(&mut bytes, FAB_BINDING);
        cbor_text(&mut bytes, "licence_id");
        cbor_text(&mut bytes, FAB_LICENCE);
        cbor_text(&mut bytes, "not_after");
        cbor_uint(&mut bytes, not_after);
        cbor_text(&mut bytes, "serial");
        cbor_text(&mut bytes, self.lease_serial());
        let sig = self.intermediate.sign(&bytes);
        ServerLease {
            serial: self.lease_serial().to_owned(),
            licence_id: Some(FAB_LICENCE.to_owned()),
            instance_binding_id: Some(FAB_BINDING.to_owned()),
            not_after,
            signature: hex::encode(sig.to_bytes()),
            signer_key_id: FAB_KID.to_owned(),
            lease_bytes: base64::engine::general_purpose::STANDARD.encode(&bytes),
        }
    }
}

/// A CBOR scalar for the hand-built omitting encoder.
enum CborScalar {
    Text(String),
    Uint(i64),
}

/// Encode a (possibly negative) CBOR integer canonically: major 0 for `n >= 0`,
/// major 1 (value `-1 - n`) for negatives.
fn cbor_int(out: &mut Vec<u8>, n: i64) {
    if n >= 0 {
        cbor_head(out, 0, n.unsigned_abs());
    } else {
        cbor_head(out, 1, (-1 - n).unsigned_abs());
    }
}

/// Minimal CBOR helpers for `sign_body_omitting` (RFC 8949 §4.2.1 shortest form).
fn cbor_map_head(out: &mut Vec<u8>, n: usize) {
    cbor_head(out, 5, n as u64);
}
fn cbor_text(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}
fn cbor_uint(out: &mut Vec<u8>, n: i64) {
    cbor_head(out, 0, u64::try_from(n).unwrap_or(0));
}
fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let mt = major << 5;
    if n < 24 {
        out.push(mt | (n as u8));
    } else if let Ok(b) = u8::try_from(n) {
        out.push(mt | 0x18);
        out.push(b);
    } else if let Ok(b) = u16::try_from(n) {
        out.push(mt | 0x19);
        out.extend_from_slice(&b.to_be_bytes());
    } else if let Ok(b) = u32::try_from(n) {
        out.push(mt | 0x1a);
        out.extend_from_slice(&b.to_be_bytes());
    } else {
        out.push(mt | 0x1b);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

/// An in-process [`LicenceServer`]: hands out the fabricated keyset and a
/// locally-signed lease, with knobs to drive the never-off-air cases (return
/// `lease: null` like a revoked entitlement; stall; or fail every call).
pub struct FakeLicenceServer {
    kit: FabricatedKeyset,
    /// When true, `heartbeat` returns `lease: null` + a `revoked` state (the
    /// revocation-by-non-reissue case): the client must keep last-good, never
    /// tighten on its own.
    withhold_lease: AtomicBool,
    /// When true, every call returns a transport error (server unreachable).
    fail: AtomicBool,
    /// When true, every call **blocks in-flight forever** (awaits `unblock`) — a
    /// real stalled transport (black hole), NOT an instant error. Used to prove a
    /// stalled heartbeat call cannot block a concurrent store reader / the output
    /// clock (invariant #10).
    block: AtomicBool,
    /// Notified to release any in-flight blocked call (so the test can tear down).
    unblock: tokio::sync::Notify,
    /// Set true once a blocked call is actually in flight (the reader waits for
    /// this so it observes a genuinely concurrent stall, not a race).
    in_flight: AtomicBool,
    /// When true, `heartbeat` replays an OLDER, already-expired (but still
    /// Ed25519-valid) signed lease — the replay-attack case: the installer must
    /// not re-extend entitlement from the replay instant.
    replay_expired: AtomicBool,
    /// When true, the returned (correctly-signed) lease binds a DIFFERENT device's
    /// instance binding id — the cross-instance replay case.
    foreign_binding: AtomicBool,
    /// When true, the returned (correctly-signed) lease's `not_after` is an
    /// absolute instant in 1970 — clearly past ANY real clock, so install() takes
    /// the `LeaseExpired` path deterministically.
    replay_absolute_past: AtomicBool,
    /// The `bindingId` the most recent heartbeat request carried (so a test can
    /// assert the renewal addresses the binding by id, never the lease serial).
    last_binding_id: std::sync::Mutex<Option<String>>,
    /// Counts heartbeat calls (the loop test asserts it advances).
    pub heartbeats: AtomicU64,
    /// The `nextDue` (epoch ms) the server returns — the loop sleeps to it.
    next_due_ms: AtomicU64,
}

impl FakeLicenceServer {
    pub fn new() -> Self {
        Self {
            kit: FabricatedKeyset::new(),
            withhold_lease: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            block: AtomicBool::new(false),
            unblock: tokio::sync::Notify::new(),
            in_flight: AtomicBool::new(false),
            replay_expired: AtomicBool::new(false),
            foreign_binding: AtomicBool::new(false),
            replay_absolute_past: AtomicBool::new(false),
            last_binding_id: std::sync::Mutex::new(None),
            heartbeats: AtomicU64::new(0),
            next_due_ms: AtomicU64::new(FAB_NOW_MS as u64 + 30 * 86_400_000),
        }
    }

    pub fn kit(&self) -> &FabricatedKeyset {
        &self.kit
    }
    pub fn pinned_root(&self) -> PinnedRoot {
        self.kit.pinned_root()
    }
    pub fn set_withhold_lease(&self, on: bool) {
        self.withhold_lease.store(on, Ordering::SeqCst);
    }
    pub fn set_fail(&self, on: bool) {
        self.fail.store(on, Ordering::SeqCst);
    }
    /// Make every server call BLOCK in-flight forever (until [`release`]) — a real
    /// stalled transport for the isolation gate.
    pub fn set_block(&self, on: bool) {
        self.block.store(on, Ordering::SeqCst);
    }
    /// True once a blocked call is genuinely in flight (await this before probing
    /// concurrency so the stall is real, not a race).
    pub fn is_in_flight(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst)
    }
    /// Release any in-flight blocked call (test teardown).
    pub fn release(&self) {
        self.block.store(false, Ordering::SeqCst);
        self.unblock.notify_waiters();
    }
    pub fn set_next_due_ms(&self, ms: u64) {
        self.next_due_ms.store(ms, Ordering::SeqCst);
    }
    /// Make `heartbeat` replay an OLDER, already-expired (but still-signed) lease.
    pub fn set_replay_expired(&self, on: bool) {
        self.replay_expired.store(on, Ordering::SeqCst);
    }
    /// Make the returned lease bind a DIFFERENT device's instance binding id.
    pub fn set_foreign_binding(&self, on: bool) {
        self.foreign_binding.store(on, Ordering::SeqCst);
    }
    /// Make the returned lease's signed `not_after` an absolute 1970 instant
    /// (deterministically past the real clock install() reads → `LeaseExpired`).
    pub fn set_replay_absolute_past(&self, on: bool) {
        self.replay_absolute_past.store(on, Ordering::SeqCst);
    }
    /// The binding id the current knobs select for the served lease body.
    fn served_binding(&self) -> &'static str {
        if self.foreign_binding.load(Ordering::SeqCst) {
            self.kit.foreign_binding_id()
        } else {
            self.kit.binding_id()
        }
    }
    /// The signed lease the current knobs select (foreign binding and/or an
    /// absolute-past expiry), else a healthy 35-day lease for this device.
    fn served_lease(&self) -> ServerLease {
        let not_after = if self.replay_absolute_past.load(Ordering::SeqCst) {
            1_000_000 // 1970-01-01T00:16:40Z — before any real clock
        } else {
            self.kit.now_ms() + 35 * 86_400_000
        };
        self.kit.sign_lease_for(self.served_binding(), not_after)
    }
    /// The bindingId the most recent heartbeat request carried.
    pub fn last_binding_id(&self) -> Option<String> {
        self.last_binding_id.lock().expect("poisoned").clone()
    }
    pub fn clear_last_binding_id(&self) {
        *self.last_binding_id.lock().expect("poisoned") = None;
    }

    /// Block in-flight (mark in_flight, await the unblock) when the block knob is
    /// set — the shared body each server method runs first.
    async fn maybe_block(&self) {
        if self.block.load(Ordering::SeqCst) {
            self.in_flight.store(true, Ordering::SeqCst);
            // Park until released. `notify_waiters` only wakes current waiters, so
            // register interest, then re-check the flag to avoid a lost wakeup.
            while self.block.load(Ordering::SeqCst) {
                let fut = self.unblock.notified();
                if !self.block.load(Ordering::SeqCst) {
                    break;
                }
                fut.await;
            }
            self.in_flight.store(false, Ordering::SeqCst);
        }
    }
}

impl LicenceServer for FakeLicenceServer {
    async fn fetch_keys(&self) -> Result<LicensingKeys, HeartbeatError> {
        self.maybe_block().await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        Ok(self.kit.keys())
    }

    async fn activate(
        &self,
        _org: &str,
        _req: ActivateRequest,
        _idempotency_key: &str,
    ) -> Result<ActivateResponse, HeartbeatError> {
        self.maybe_block().await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        Ok(ActivateResponse::new(
            Some(self.served_lease()),
            EnforcementState::Compliant,
        ))
    }

    async fn heartbeat(
        &self,
        _org: &str,
        req: HeartbeatRequest,
        _idempotency_key: &str,
    ) -> Result<HeartbeatResponse, HeartbeatError> {
        self.maybe_block().await;
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        // Record the bindingId the client addressed us by (the renewal must use
        // the server's instanceBindingId, never the lease serial).
        *self.last_binding_id.lock().expect("poisoned") = Some(req.binding_id.clone());
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        let (lease, state) = if self.withhold_lease.load(Ordering::SeqCst) {
            (None, EnforcementState::Revoked)
        } else if self.replay_expired.load(Ordering::SeqCst) {
            // A replay: an OLDER lease with a DISTINCT (older) serial, still
            // Ed25519-valid, whose signed not_after is already in the past
            // (expired 5 days ago). A distinct serial means the install is NOT a
            // benign serial-collision no-op — the ONLY thing that can keep it from
            // re-extending entitlement is honouring the signed (past) not_after.
            let past = self.kit.now_ms() - 5 * 86_400_000;
            (
                Some(self.kit.sign_old_lease(past)),
                EnforcementState::Compliant,
            )
        } else {
            // The knob-selected lease (foreign binding and/or absolute-past expiry,
            // else a healthy 35-day lease for this device).
            (Some(self.served_lease()), EnforcementState::Compliant)
        };
        Ok(HeartbeatResponse::new(
            lease,
            state,
            self.next_due_ms.load(Ordering::SeqCst) as i64,
        ))
    }
}

/// Shareable handle for the loop test.
pub fn shared_fake() -> Arc<FakeLicenceServer> {
    Arc::new(FakeLicenceServer::new())
}
