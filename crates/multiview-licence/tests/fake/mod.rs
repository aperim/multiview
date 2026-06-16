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
        let pubkey = self.intermediate.verifying_key().to_bytes().to_vec();
        let pre = canonical_key_preimage(
            FAB_KID,
            "lease",
            "conspect.key-attestation.v1",
            &pubkey,
            FAB_VALID_FROM,
            FAB_VALID_UNTIL,
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
                "key_type": "lease",
                "algorithm": "ed25519",
                "public_key": self.intermediate_pub_b64url(),
                "valid_from": FAB_VALID_FROM,
                "valid_until": FAB_VALID_UNTIL,
                "status": "current",
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

    /// Mint a signed lease (bare Ed25519 over a canonical-CBOR body), `term_days`
    /// out from `now_ms`.
    pub fn sign_lease(&self, term_days: i64) -> ServerLease {
        let not_after = self.now_ms() + term_days * 86_400_000;
        let body = LeaseBodyFields {
            licence_id: FAB_LICENCE.to_owned(),
            instance_binding_id: FAB_BINDING.to_owned(),
            serial: "01931d4e-7b2a-7c00-9f3a-2b6e1c0e9b41".to_owned(),
            not_after,
            gpu_limit: Some(2),
            hardware_class: Some("standard".to_owned()),
        };
        let lease_bytes = body.to_canonical_cbor();
        let sig = self.intermediate.sign(&lease_bytes);
        ServerLease {
            serial: body.serial.clone(),
            licence_id: Some(body.licence_id.clone()),
            instance_binding_id: Some(body.instance_binding_id.clone()),
            not_after,
            signature: hex::encode(sig.to_bytes()),
            signer_key_id: FAB_KID.to_owned(),
            lease_bytes: base64::engine::general_purpose::STANDARD.encode(&lease_bytes),
        }
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
    pub fn set_next_due_ms(&self, ms: u64) {
        self.next_due_ms.store(ms, Ordering::SeqCst);
    }
}

impl LicenceServer for FakeLicenceServer {
    async fn fetch_keys(&self) -> Result<LicensingKeys, HeartbeatError> {
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
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        Ok(ActivateResponse::new(
            Some(self.kit.sign_lease(35)),
            EnforcementState::Compliant,
        ))
    }

    async fn heartbeat(
        &self,
        _org: &str,
        _req: HeartbeatRequest,
        _idempotency_key: &str,
    ) -> Result<HeartbeatResponse, HeartbeatError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(HeartbeatError::Transport("fake offline".to_owned()));
        }
        self.heartbeats.fetch_add(1, Ordering::SeqCst);
        let (lease, state) = if self.withhold_lease.load(Ordering::SeqCst) {
            (None, EnforcementState::Revoked)
        } else {
            (Some(self.kit.sign_lease(35)), EnforcementState::Compliant)
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
