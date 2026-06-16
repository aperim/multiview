//! The Conspect **device-licensing heartbeat client** (CONSPECT-3, ADR-0096) —
//! feature `heartbeat`, OFF by default.
//!
//! This module is the device→server licensing client: it fetches the published
//! key-trust material, verifies the **pinned-root → attested-intermediate** chain
//! (D1), verifies a server lease's **bare-Ed25519** signature over the
//! deterministic-CBOR lease body (D3), and drives the existing
//! [`LeaseStore::install_binding`](crate::store::LeaseStore::install_binding)
//! convergence so the rest of the machine (S1/S2/S3, the control routes, the web
//! screens) reads the renewed lease with no extra wiring.
//!
//! # Never off air (invariants #1 / #10)
//!
//! The client only ever **tightens** on a positively-verified signed lease.
//! Every failure mode — an unreachable server, a failed renew, a malformed
//! response, a suspect clock, **and a deliberately withheld lease** (revocation
//! by non-reissue: a `200` with `lease: null`) — leaves the last-good lease in
//! place to age via the existing ladder. There is no push kill verb and no code
//! path here that removes or downgrades a lease on its own. The
//! [`HeartbeatClient`] holds only an [`Arc<LeaseStore>`](crate::store::LeaseStore),
//! the pinned root, the server handle, and config — **never** an engine handle,
//! channel, or lock the data plane takes — so it is physically unable to
//! back-pressure or stall the engine. Its sole side effect is `install_binding`.
//!
//! # Trust posture (fail closed on trust, lenient on enforcement)
//!
//! Trust material is verified strictly: an un-attested intermediate, a forged
//! revocation list, or a bad lease signature is **rejected** (never trusted). But
//! a *rejection* never degrades the machine — it simply withholds the next lease,
//! and the previous lease ages normally. The two postures compose: we are
//! paranoid about *what we trust*, lenient about *what we enforce*.
//!
//! # Auth (today)
//!
//! Requests carry the account-JWT `Authorization: Bearer` chain (the
//! device proof-of-possession request-signing wire format is deferred
//! server-side, ADR-0096 D2). The
//! [`DeviceIdentity::device_public_key_b64url`] is captured and stored on the
//! binding but does **not** authenticate requests yet. Every mutation carries a
//! required `Idempotency-Key` (a retry replays, never re-issues).
//!
//! The live HTTP transport lives at the cli/app boundary (it owns `reqwest`); the
//! testable crypto + the loop live here behind the [`LicenceServer`] seam, so this
//! leaf crate never opens a socket of its own.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ed25519_dalek::{Signature as EdSignature, VerifyingKey as EdVerifyingKey};
use p256::ecdsa::{signature::Verifier as _, Signature as P256Signature, VerifyingKey as P256Key};
use serde::{Deserialize, Serialize};

use crate::constants::ACTIVATION_WINDOW_DAYS;
use crate::entitlement::{Entitlement, EntitlementFlags, GpuLimit, HardwareClass, Tier};
use crate::lease::Lease;
use crate::store::{system_now, LeaseBinding, LeaseStore};
use crate::verify::{PinnedKey, SignedLease};

// ===========================================================================
// Canonical CBOR (RFC 8949 §4.2.1) — the key pre-image + revocation pre-image.
// ===========================================================================
//
// A tiny, total, hand-rolled canonical encoder for the fixed-shape attestation
// pre-images. We do NOT rely on a serde-CBOR codec's map-key ordering: `ciborium`
// preserves insertion order rather than sorting keys, so a canonical guarantee
// must be ours. The pre-image maps are fixed: their keys, written in the
// well-known `key_pre_image` order, are already in RFC 8949 §4.2.1 canonical
// order (shortest-encoded-key first, then bytewise) — verified byte-exact against
// the live `root_sig` that signs exactly these bytes (see the golden-vector test).

/// Append a CBOR head (major type + argument) using the shortest encoding
/// (preferred serialization, RFC 8949 §4.2.1). `n` is the unsigned argument.
fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let mt = major << 5;
    if n < 24 {
        out.push(mt | u8::try_from(n).unwrap_or(0));
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

/// The CBOR head argument for a `len` (a collection length / byte count). A
/// length beyond `u64` cannot occur for these small fixed pre-images; it clamps
/// to `u64::MAX` rather than `as`-truncating (total + panic-free).
fn len_arg(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

/// Append a CBOR text string (major 3).
fn cbor_tstr(out: &mut Vec<u8>, s: &str) {
    cbor_head(out, 3, len_arg(s.len()));
    out.extend_from_slice(s.as_bytes());
}

/// Append a CBOR byte string (major 2).
fn cbor_bstr(out: &mut Vec<u8>, b: &[u8]) {
    cbor_head(out, 2, len_arg(b.len()));
    out.extend_from_slice(b);
}

/// Append a CBOR unsigned integer (major 0). The pre-image times are epoch-ms
/// (always non-negative); a negative input clamps to 0 (it cannot occur for a
/// valid timestamp and is never silently mis-encoded as a different number).
fn cbor_uint(out: &mut Vec<u8>, n: i64) {
    cbor_head(out, 0, u64::try_from(n).unwrap_or(0));
}

/// The deterministic-CBOR **key pre-image** the well-known `root_sig` covers
/// (ADR-0096 D3): a `map(6)` over `[key_id, key_type, statement,
/// public_key(bstr), valid_from(uint), valid_until(uint)]` in that (canonical)
/// order. `public_key` is the **raw 32-byte Ed25519 point** (a CBOR byte string),
/// the times are epoch-milliseconds.
///
/// Exposed (crate-public via re-export) so tests can build the exact pre-image a
/// fabricated root signs.
#[must_use]
pub fn canonical_key_preimage(
    key_id: &str,
    key_type: &str,
    statement: &str,
    public_key: &[u8],
    valid_from: i64,
    valid_until: i64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(160);
    cbor_head(&mut out, 5, 6); // map(6)
    cbor_tstr(&mut out, "key_id");
    cbor_tstr(&mut out, key_id);
    cbor_tstr(&mut out, "key_type");
    cbor_tstr(&mut out, key_type);
    cbor_tstr(&mut out, "statement");
    cbor_tstr(&mut out, statement);
    cbor_tstr(&mut out, "public_key");
    cbor_bstr(&mut out, public_key);
    cbor_tstr(&mut out, "valid_from");
    cbor_uint(&mut out, valid_from);
    cbor_tstr(&mut out, "valid_until");
    cbor_uint(&mut out, valid_until);
    out
}

/// The deterministic-CBOR **revocation pre-image** the `root_revocation_sig`
/// covers (ADR-0096 D3): a `map(3)` over `[issued_at(uint), statement,
/// revoked_key_ids(array of tstr)]` in that (canonical) order.
#[must_use]
pub fn canonical_revocation_preimage(
    issued_at: i64,
    statement: &str,
    revoked_key_ids: &[String],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    cbor_head(&mut out, 5, 3); // map(3)
    cbor_tstr(&mut out, "issued_at");
    cbor_uint(&mut out, issued_at);
    cbor_tstr(&mut out, "statement");
    cbor_tstr(&mut out, statement);
    cbor_tstr(&mut out, "revoked_key_ids");
    cbor_head(&mut out, 4, len_arg(revoked_key_ids.len())); // array(n)
    for id in revoked_key_ids {
        cbor_tstr(&mut out, id);
    }
    out
}

// ===========================================================================
// The published well-known key-trust document.
// ===========================================================================

/// The well-known licensing-keys document
/// (`GET /.well-known/conspect-licensing-keys.json`). Deserialised as received;
/// only the fields the verifier needs are modelled (`#[serde(default)]` on the
/// rest keeps it tolerant of additive server changes — but the cryptographic
/// fields are required).
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct LicensingKeys {
    /// The pinned trust anchor descriptor (its key bytes are compared to the
    /// build-pinned [`PinnedRoot`]; a mismatch is rejected).
    pub root: RootDescriptor,
    /// The attestation contract (statements + encoding); the verifier reads the
    /// statement strings from here.
    pub attestation_contract: AttestationContract,
    /// The dual-pin lease-signing intermediates.
    pub lease_keys: Vec<IntermediateKey>,
    /// The revocation list + its root signature.
    pub revocation: Revocation,
}

/// The root descriptor in the well-known document.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct RootDescriptor {
    /// The root key id (informational).
    #[serde(default)]
    pub kid: String,
    /// The base64url-encoded uncompressed P-256 point.
    pub public_key: String,
}

/// The attestation contract block.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct AttestationContract {
    /// The statement string carried in each key pre-image.
    pub key_statement: String,
    /// The statement string carried in the revocation pre-image.
    pub revocation_statement: String,
}

/// One dual-pin intermediate key in the well-known document.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct IntermediateKey {
    /// The key id leases reference via `signerKeyId`.
    pub kid: String,
    /// The key kind (`"lease"`).
    pub key_type: String,
    /// The base64url-encoded raw 32-byte Ed25519 public key.
    pub public_key: String,
    /// The validity start (epoch milliseconds).
    pub valid_from: i64,
    /// The validity end (epoch milliseconds).
    pub valid_until: i64,
    /// `"current"` or `"next"` (both accepted within validity — dual-pin).
    #[serde(default)]
    pub status: String,
    /// The base64url-encoded ECDSA-P256 `root_sig` (raw r||s) over the key
    /// pre-image.
    pub root_sig: String,
}

/// The revocation block.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Revocation {
    /// The revocation statement (must match `attestation_contract`).
    #[serde(default)]
    pub statement: String,
    /// When the revocation list was issued (epoch milliseconds).
    pub issued_at: i64,
    /// The revoked key ids (their leases/keys are dropped from trust).
    #[serde(default)]
    pub revoked_key_ids: Vec<String>,
    /// The base64url-encoded ECDSA-P256 signature over the revocation pre-image.
    pub root_revocation_sig: String,
}

// ===========================================================================
// Key trust: pinned root → attested intermediates → dual-pin + revocation.
// ===========================================================================

/// The build-pinned root verifying key (ECDSA P-256). Obtained out-of-band and
/// pinned in the binary/config; this crate **verifies against** it, never mints
/// it.
#[derive(Debug, Clone)]
pub struct PinnedRoot {
    key: P256Key,
    /// The SEC1 uncompressed encoding, retained so we can reject a well-known
    /// document whose advertised root does not byte-match the pinned anchor.
    sec1: Vec<u8>,
}

impl PinnedRoot {
    /// Pin a root from a SEC1 point encoding (uncompressed `0x04||X||Y`, 65
    /// bytes; a compressed point is also accepted by the parser).
    ///
    /// # Errors
    /// [`KeyTrustError::MalformedRoot`] if the bytes are not a valid P-256 point.
    pub fn from_sec1_bytes(bytes: &[u8]) -> Result<Self, KeyTrustError> {
        let key = P256Key::from_sec1_bytes(bytes).map_err(|_| KeyTrustError::MalformedRoot)?;
        Ok(Self {
            key,
            sec1: bytes.to_vec(),
        })
    }

    /// Pin a root from the base64url-encoded uncompressed point the well-known
    /// document publishes (`base64url-uncompressed-p256-point`).
    ///
    /// # Errors
    /// [`KeyTrustError::MalformedRoot`] on a bad base64url string or point.
    pub fn from_base64url(s: &str) -> Result<Self, KeyTrustError> {
        let bytes = b64url(s).ok_or(KeyTrustError::MalformedRoot)?;
        Self::from_sec1_bytes(&bytes)
    }

    /// Verify an ECDSA-P256/SHA-256 signature (raw r||s) over `message`.
    fn verify(&self, message: &[u8], raw_sig: &[u8]) -> bool {
        match P256Signature::from_slice(raw_sig) {
            Ok(sig) => self.key.verify(message, &sig).is_ok(),
            Err(_) => false,
        }
    }
}

/// The verified, in-validity set of lease-signing keys: the only keys a server
/// lease may be signed under. Built by [`TrustedKeys::verify`]; resolving an
/// unknown or revoked or out-of-validity `signerKeyId` yields `None`.
#[derive(Debug, Clone)]
pub struct TrustedKeys {
    keys: Vec<(String, EdVerifyingKey)>,
}

impl TrustedKeys {
    /// Verify a well-known document against the pinned root and return the set of
    /// trusted lease-signing keys valid at `now_ms` (epoch milliseconds).
    ///
    /// Steps (all must pass, else the whole keyset is rejected):
    /// 1. the document's advertised root byte-matches the pinned anchor;
    /// 2. the revocation list is itself root-attested (fail closed: a forged
    ///    revocation signature is rejected, never silently honoured/ignored);
    /// 3. each intermediate's `root_sig` verifies over its canonical key
    ///    pre-image against the pinned root.
    ///
    /// Then the trusted set is those attested intermediates that are **within
    /// their validity window** at `now_ms` and **not** named in the (verified)
    /// revocation list. Both `current` and `next` status keys are accepted
    /// (dual-pin), so a key rotation never strands a fielded build.
    ///
    /// # Errors
    /// [`KeyTrustError`] when the pinned root mismatches, the revocation list is
    /// not root-attested, or any intermediate is not root-attested.
    pub fn verify(
        keys: &LicensingKeys,
        pinned: &PinnedRoot,
        now_ms: i64,
    ) -> Result<Self, KeyTrustError> {
        // 1. The advertised root must be the pinned anchor (defends against a
        //    substituted well-known document with a self-consistent foreign root).
        let advertised = b64url(&keys.root.public_key).ok_or(KeyTrustError::MalformedRoot)?;
        let advertised_root =
            PinnedRoot::from_sec1_bytes(&advertised).map_err(|_| KeyTrustError::MalformedRoot)?;
        if advertised_root.sec1 != pinned.sec1 {
            return Err(KeyTrustError::RootMismatch);
        }

        // 2. The revocation list must be root-attested before we honour it.
        let rev_pre = canonical_revocation_preimage(
            keys.revocation.issued_at,
            &keys.attestation_contract.revocation_statement,
            &keys.revocation.revoked_key_ids,
        );
        let rev_sig = b64url(&keys.revocation.root_revocation_sig)
            .ok_or(KeyTrustError::RevocationNotAttested)?;
        if !pinned.verify(&rev_pre, &rev_sig) {
            return Err(KeyTrustError::RevocationNotAttested);
        }
        let revoked = &keys.revocation.revoked_key_ids;

        // 3. Each intermediate's root_sig must verify over its canonical pre-image.
        let mut trusted = Vec::new();
        for ik in &keys.lease_keys {
            let pubkey =
                b64url(&ik.public_key).ok_or_else(|| KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                })?;
            // Reject a non-sensical (negative) validity window up front: a negative
            // epoch-ms cannot be a real Conspect timestamp, and the canonical-CBOR
            // uint encoder would otherwise coerce it (a forged/garbled key must not
            // be silently normalised into something the root could appear to sign).
            if ik.valid_from < 0 || ik.valid_until < 0 {
                return Err(KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                });
            }
            let pre = canonical_key_preimage(
                &ik.kid,
                &ik.key_type,
                &keys.attestation_contract.key_statement,
                &pubkey,
                ik.valid_from,
                ik.valid_until,
            );
            let sig =
                b64url(&ik.root_sig).ok_or_else(|| KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                })?;
            if !pinned.verify(&pre, &sig) {
                return Err(KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                });
            }
            // Attested — but attestation is PURPOSE-BOUND. Only a key whose
            // root-signed pre-image declared `key_type == "lease"` is a lease
            // signer; a root-attested key minted for any other purpose (e.g. an
            // update/signing key) must NOT be accepted to sign leases. `key_type`
            // IS in the signed pre-image, so this gate is cryptographically bound.
            // A SKIP, not a hard reject: an unrelated non-lease key in the document
            // does not poison the whole keyset, it is simply never a lease signer.
            if ik.key_type != "lease" {
                continue;
            }
            // The trust decision rests ONLY on signed fields. `ik.status` is NOT in
            // the root-signed pre-image (a MITM / compromised well-known doc can
            // flip a retired key's status to "current" without breaking root_sig),
            // so it is a non-binding operational hint and MUST NOT gate trust.
            // Retirement is expressed via the SIGNED revocation list and the SIGNED
            // validity window — both checked below.
            if revoked.iter().any(|r| r == &ik.kid) {
                continue;
            }
            if now_ms < ik.valid_from || now_ms > ik.valid_until {
                continue;
            }
            let arr: [u8; 32] = match pubkey.as_slice().try_into() {
                Ok(a) => a,
                Err(_) => {
                    return Err(KeyTrustError::IntermediateNotAttested {
                        kid: ik.kid.clone(),
                    })
                }
            };
            let vk = EdVerifyingKey::from_bytes(&arr).map_err(|_| {
                KeyTrustError::IntermediateNotAttested {
                    kid: ik.kid.clone(),
                }
            })?;
            trusted.push((ik.kid.clone(), vk));
        }
        Ok(Self { keys: trusted })
    }

    /// The trusted Ed25519 verifying key for `signer_key_id`, if it is attested,
    /// in-validity, and not revoked.
    #[must_use]
    pub fn lease_key(&self, signer_key_id: &str) -> Option<&EdVerifyingKey> {
        self.keys
            .iter()
            .find(|(kid, _)| kid == signer_key_id)
            .map(|(_, vk)| vk)
    }
}

/// Why the key-trust chain was rejected. Every variant means "do not trust this
/// material" — never "tighten enforcement"; a rejection withholds the next lease,
/// it never downgrades the machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyTrustError {
    /// The pinned root bytes are not a valid P-256 point.
    #[error("the pinned root key is malformed")]
    MalformedRoot,
    /// The well-known document advertises a root that is not the pinned anchor.
    #[error("the advertised root key does not match the pinned anchor")]
    RootMismatch,
    /// The revocation list's signature does not verify against the pinned root
    /// (fail closed — we never honour an unsigned revocation list).
    #[error("the revocation list is not root-attested")]
    RevocationNotAttested,
    /// An intermediate's `root_sig` does not verify over its canonical pre-image.
    #[error("intermediate {kid} is not root-attested")]
    IntermediateNotAttested {
        /// The offending intermediate's key id.
        kid: String,
    },
}

// ===========================================================================
// The signed server lease (bare Ed25519 over deterministic-CBOR leaseBytes).
// ===========================================================================

/// A signed lease as returned by `activate`/`heartbeat` (the shared shape;
/// `licenceId`/`instanceBindingId` are present only on activation). The
/// `leaseBytes` are the authoritative signed body; the scalar fields are a
/// convenience subset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerLease {
    /// The signer-minted lease serial (`UUIDv7`).
    pub serial: String,
    /// The licence this lease was issued against (activation only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub licence_id: Option<String>,
    /// The instance binding the lease is bound to (activation only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_binding_id: Option<String>,
    /// The lease expiry (epoch milliseconds) — the convenience subset of the
    /// signed `not_after`.
    pub not_after: i64,
    /// The bare Ed25519 signature, lower-case hex (64 bytes / 128 hex chars).
    pub signature: String,
    /// The dual-pin intermediate key id the lease was signed under.
    pub signer_key_id: String,
    /// The exact canonical-CBOR lease body the signature covers,
    /// **STANDARD-base64** (RFC 4648 §4, NOT base64url).
    pub lease_bytes: String,
}

/// The authoritative fields parsed out of the signed CBOR lease body — the
/// offline-enforcement inputs (`gpu_limit`, `hardware_class`, `not_after`, the
/// chain identity). Only the fields the install path needs are modelled.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseBody {
    /// The licence id (from the signed body).
    pub licence_id: String,
    /// The instance binding id (from the signed body).
    pub instance_binding_id: String,
    /// The lease serial (from the signed body).
    pub serial: String,
    /// The lease expiry (epoch milliseconds, from the signed body).
    pub not_after: i64,
    /// The GPU allowance carried in the signed body (`None` if absent).
    pub gpu_limit: Option<u32>,
    /// The licensed hardware class string carried in the signed body.
    pub hardware_class: Option<String>,
}

impl LeaseBody {
    /// Whether the signed lease is already expired at `now_ms` (epoch ms): its
    /// cryptographically-signed `not_after` is at or before `now_ms`. The
    /// installer rejects such a lease (a signed-but-expired or replayed-old lease
    /// must never become a fresh active term).
    #[must_use]
    pub fn is_expired_at(&self, now_ms: i64) -> bool {
        self.not_after <= now_ms
    }
}

/// The test/fabrication mirror of [`LeaseBody`] with a deterministic-CBOR encoder
/// (so a fake server can mint a body and sign exactly the bytes the verifier
/// re-checks). Field order is the canonical map order used on the wire.
#[derive(Debug, Clone)]
pub struct LeaseBodyFields {
    /// The licence id.
    pub licence_id: String,
    /// The instance binding id.
    pub instance_binding_id: String,
    /// The lease serial.
    pub serial: String,
    /// The lease expiry (epoch milliseconds).
    pub not_after: i64,
    /// The GPU allowance.
    pub gpu_limit: Option<u32>,
    /// The licensed hardware class.
    pub hardware_class: Option<String>,
}

impl LeaseBodyFields {
    /// Encode this body as deterministic CBOR (canonical map order). The optional
    /// fields are omitted when `None`, so the map count adapts; the present keys
    /// are written in `[gpu_limit, hardware_class, instance_binding_id,
    /// licence_id, not_after, serial]` — i.e. the keys sorted in RFC 8949 §4.2.1
    /// canonical order (all are length-distinct or bytewise-ordered text keys).
    #[must_use]
    pub fn to_canonical_cbor(&self) -> Vec<u8> {
        // Collect present (key, value) pairs, sort by canonical key order, emit.
        // This keeps the encoder honest about canonicality regardless of the
        // struct field order.
        let mut pairs: Vec<(&str, CborVal)> = Vec::with_capacity(6);
        if let Some(g) = self.gpu_limit {
            pairs.push(("gpu_limit", CborVal::Uint(i64::from(g))));
        }
        if let Some(hc) = self.hardware_class.clone() {
            pairs.push(("hardware_class", CborVal::Text(hc)));
        }
        pairs.push((
            "instance_binding_id",
            CborVal::Text(self.instance_binding_id.clone()),
        ));
        pairs.push(("licence_id", CborVal::Text(self.licence_id.clone())));
        pairs.push(("not_after", CborVal::Uint(self.not_after)));
        pairs.push(("serial", CborVal::Text(self.serial.clone())));
        pairs.sort_by(|a, b| canonical_key_cmp(a.0, b.0));

        let mut out = Vec::with_capacity(128);
        cbor_head(&mut out, 5, len_arg(pairs.len()));
        for (k, v) in &pairs {
            cbor_tstr(&mut out, k);
            match v {
                CborVal::Uint(n) => cbor_uint(&mut out, *n),
                CborVal::Text(s) => cbor_tstr(&mut out, s),
            }
        }
        out
    }
}

/// A CBOR scalar value for the canonical lease-body encoder.
enum CborVal {
    /// An unsigned integer (major 0).
    Uint(i64),
    /// A text string (major 3).
    Text(String),
}

/// Compare two text map keys by RFC 8949 §4.2.1 order: shorter encoded key first
/// (for text strings of equal short length that means by length then bytewise),
/// then bytewise on the encoded bytes.
fn canonical_key_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.len()
        .cmp(&b.len())
        .then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// Verify a [`ServerLease`] end to end: resolve `signer_key_id` to a trusted
/// intermediate, then verify the **bare Ed25519** signature over the
/// **standard-base64-decoded** `lease_bytes`, then parse the CBOR body.
///
/// # Errors
/// [`SignedLeaseError`] for an unknown signer, a malformed signature/body, a
/// signature that does not verify, or a body missing required fields.
pub fn verify_signed_lease_chain(
    lease: &ServerLease,
    trusted: &TrustedKeys,
) -> Result<LeaseBody, SignedLeaseError> {
    let vk =
        trusted
            .lease_key(&lease.signer_key_id)
            .ok_or_else(|| SignedLeaseError::UnknownSigner {
                kid: lease.signer_key_id.clone(),
            })?;
    let sig_bytes =
        hex::decode(lease.signature.trim()).map_err(|_| SignedLeaseError::MalformedSignature)?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| SignedLeaseError::MalformedSignature)?;
    let signature = EdSignature::from_bytes(&sig_arr);
    // Decode the received STANDARD-base64 (RFC 4648 §4) EXACTLY — the signature
    // covers these bytes. Do NOT strip '=' first: a canonically-padded body (CBOR
    // length % 3 != 0) carries real padding, and stripping it makes the input
    // non-canonical so `STANDARD` (RequireCanonical) would wrongly reject a valid
    // lease. `.trim()` only removes incidental surrounding whitespace.
    let body_bytes = base64::engine::general_purpose::STANDARD
        .decode(lease.lease_bytes.trim())
        .map_err(|_| SignedLeaseError::MalformedBody)?;
    vk.verify_strict(&body_bytes, &signature)
        .map_err(|_| SignedLeaseError::BadSignature)?;
    parse_lease_body(&body_bytes)
}

/// Parse the signed CBOR lease body into the fields the install path needs. A
/// missing required field is a typed error (never a panic).
fn parse_lease_body(bytes: &[u8]) -> Result<LeaseBody, SignedLeaseError> {
    let value: ciborium::value::Value =
        ciborium::from_reader(bytes).map_err(|_| SignedLeaseError::MalformedBody)?;
    let map = value.as_map().ok_or(SignedLeaseError::MalformedBody)?;
    let get = |key: &str| -> Option<&ciborium::value::Value> {
        map.iter()
            .find(|(k, _)| k.as_text() == Some(key))
            .map(|(_, v)| v)
    };
    // A REQUIRED text field: must be present AND non-empty. Fail closed — a
    // signed-but-malformed body (a field omitted, or present-but-empty) is
    // rejected, never installed with an empty id that could mis-bind enforcement.
    let required_text = |key: &str| -> Result<String, SignedLeaseError> {
        match get(key).and_then(ciborium::value::Value::as_text) {
            Some(s) if !s.is_empty() => Ok(s.to_owned()),
            _ => Err(SignedLeaseError::MalformedBody),
        }
    };
    let integer = |key: &str| -> Option<i64> {
        get(key)
            .and_then(ciborium::value::Value::as_integer)
            .and_then(|i| i64::try_from(i).ok())
    };
    let licence_id = required_text("licence_id")?;
    let instance_binding_id = required_text("instance_binding_id")?;
    let serial = required_text("serial")?;
    let not_after = integer("not_after").ok_or(SignedLeaseError::MalformedBody)?;
    // gpu_limit fails CLOSED: ABSENT means Unlimited, but a value that is PRESENT
    // and out of range for a u32 GPU count (negative, non-integer, or > u32::MAX)
    // is MalformedBody — never silently folded to `Unlimited` (the LEAST
    // restrictive), which would let a malformed-but-signed lease grant unlimited
    // GPUs. `as_conversions` is denied, so the bound is an explicit `u32::try_from`.
    let gpu_limit = match get("gpu_limit") {
        None => None,
        Some(value) => Some(
            value
                .as_integer()
                .and_then(|i| i64::try_from(i).ok())
                .and_then(|n| u32::try_from(n).ok())
                .ok_or(SignedLeaseError::MalformedBody)?,
        ),
    };
    let hardware_class = get("hardware_class")
        .and_then(ciborium::value::Value::as_text)
        .map(str::to_owned);
    Ok(LeaseBody {
        licence_id,
        instance_binding_id,
        serial,
        not_after,
        gpu_limit,
        hardware_class,
    })
}

/// Why a signed lease was rejected. Like [`KeyTrustError`], a rejection only ever
/// **withholds** the next lease — it never tightens the machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SignedLeaseError {
    /// The lease's `signerKeyId` is not in the trusted set (unknown, revoked, or
    /// out of validity).
    #[error("lease signer key id {kid} is not trusted")]
    UnknownSigner {
        /// The unrecognised signer key id.
        kid: String,
    },
    /// The signature field is not 64 bytes of lower-case hex.
    #[error("the lease signature is malformed")]
    MalformedSignature,
    /// The `leaseBytes` are not valid standard-base64 / CBOR.
    #[error("the signed lease body is malformed")]
    MalformedBody,
    /// The Ed25519 signature did not verify over the signed body (tamper or
    /// wrong key).
    #[error("the lease signature did not verify")]
    BadSignature,
}

// ===========================================================================
// Wire requests/responses (verbatim field names, Conspect /v0 v0.6.1).
// ===========================================================================

/// The 9-rung enforcement-ladder state, returned as **data** inside a `200`
/// (never a control verb). The client records it but never acts on it to tighten
/// — enforcement is driven by the local lease ladder ([`crate::ladder`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EnforcementState {
    /// Within the lease term.
    Compliant,
    /// In the grace window.
    Grace,
    /// Soft-lapsed (data only).
    LapsedSoft,
    /// Hard-lapsed (data only).
    LapsedHard,
    /// An evaluation/trial grant.
    Evaluation,
    /// Administratively on hold.
    OnHold,
    /// The detected hardware class does not match the licensed class.
    ClassMismatch,
    /// More GPUs in use than licensed.
    OverGpu,
    /// The entitlement is revoked (lease withheld — revocation by non-reissue).
    Revoked,
}

/// The `POST /organisations/{orgId}/activate` request body (verbatim field
/// names). Carries salted digests + opaque ids only — never a raw identifier.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivateRequest {
    /// The registered machine id this instance runs on.
    pub machine_id: String,
    /// The 6-char single-use claim code (paid order); **omitted** to auto-issue a
    /// free non-commercial licence (the free tier is itself a licence).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_code: Option<String>,
    /// The salted hardware-fingerprint digest (lower-case hex SHA-256).
    pub fingerprint_digest: String,
    /// The weighted fingerprint match score (0–100; ≥70 to activate).
    pub fingerprint_score: u8,
    /// The salted hardware digest shared by sibling instances on one machine.
    pub hardware_digest: String,
    /// The instance id (the seat-consuming, lease-bearing unit).
    pub instance_id: String,
    /// The instance discriminator hash (`SHA-256(salt ‖ instance_slug)`).
    pub instance_discriminator_hash: String,
    /// The instance discriminator digest (lower-case hex SHA-256).
    pub instance_discriminator_digest: String,
    /// The instance's Ed25519 device proof-of-possession **public** key (captured
    /// + stored; not used to authenticate requests yet — ADR-0096 D2).
    pub device_public_key: String,
    /// The server-issued lease nonce (lower-case hex) carried in the signed body.
    pub server_nonce: String,
}

/// The `POST …/activate` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ActivateResponse {
    /// The freshly-signed lease (absent when a renewal is withheld).
    #[serde(default)]
    pub lease: Option<ServerLease>,
    /// The enforcement-ladder rung (data inside the 200).
    pub enforcement_state: EnforcementState,
}

impl ActivateResponse {
    /// Assemble an activation response. A constructor is provided because the
    /// type is `#[non_exhaustive]` (a versioned wire response): the in-process
    /// fake server + the cli's transport build it explicitly.
    #[must_use]
    pub fn new(lease: Option<ServerLease>, enforcement_state: EnforcementState) -> Self {
        Self {
            lease,
            enforcement_state,
        }
    }
}

/// The `POST /organisations/{orgId}/heartbeat` request body (verbatim field
/// names) — the minimal licensing keep-alive: the binding id, the lease serial
/// head of the chain, the salted fingerprint digest, the app version, and the
/// transport. **No** raw identifier, **no** telemetry (heartbeat ≠ telemetry).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HeartbeatRequest {
    /// The instance binding id (the seat-consuming, lease-bearing unit).
    pub binding_id: String,
    /// The serial of the lease the device currently holds (head of its chain),
    /// or `null` when none is held yet.
    pub lease_serial: Option<String>,
    /// The device's salted hardware-fingerprint digest (lower-case hex).
    pub fingerprint_digest: String,
    /// The engine version reporting in.
    pub app_version: String,
    /// The channel the heartbeat arrived on (`direct`/`relay`/`file`).
    pub transport: String,
}

/// The `POST …/heartbeat` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HeartbeatResponse {
    /// The freshly-signed 35-day lease, or `null` when the entitlement is revoked
    /// (the running output continues by ageing — never off air).
    #[serde(default)]
    pub lease: Option<ServerLease>,
    /// The enforcement-ladder rung (data inside the 200).
    pub enforcement_state: EnforcementState,
    /// When the next monthly heartbeat is due (epoch milliseconds). The loop
    /// sleeps to this instant.
    pub next_due: i64,
}

impl HeartbeatResponse {
    /// Assemble a heartbeat response. A constructor is provided because the type
    /// is `#[non_exhaustive]` (a versioned wire response): the in-process fake
    /// server + the cli's transport build it explicitly.
    #[must_use]
    pub fn new(
        lease: Option<ServerLease>,
        enforcement_state: EnforcementState,
        next_due: i64,
    ) -> Self {
        Self {
            lease,
            enforcement_state,
            next_due,
        }
    }
}

// ===========================================================================
// The LicenceServer seam + the HeartbeatClient loop.
// ===========================================================================

/// The licence-server seam. Implemented by the real HTTP transport (at the
/// cli/app boundary, which owns `reqwest`) and by an in-process fake for tests.
///
/// Uses native `async fn` in trait with a `Send` future (the house pattern —
/// mirrors `multiview-control`'s `ZowietekTransport`), so the loop can be
/// `tokio::spawn`'d. The client is generic over the server (`HeartbeatClient<S>`)
/// rather than `dyn`-dispatched: the spawn point holds exactly one concrete
/// server, so generics are alloc-free and the seam stays clean.
pub trait LicenceServer: Send + Sync {
    /// Fetch the published well-known key-trust document.
    fn fetch_keys(
        &self,
    ) -> impl std::future::Future<Output = Result<LicensingKeys, HeartbeatError>> + Send;

    /// `POST /organisations/{org}/activate` with a required `Idempotency-Key`.
    fn activate(
        &self,
        org: &str,
        req: ActivateRequest,
        idempotency_key: &str,
    ) -> impl std::future::Future<Output = Result<ActivateResponse, HeartbeatError>> + Send;

    /// `POST /organisations/{org}/heartbeat` with a required `Idempotency-Key`.
    fn heartbeat(
        &self,
        org: &str,
        req: HeartbeatRequest,
        idempotency_key: &str,
    ) -> impl std::future::Future<Output = Result<HeartbeatResponse, HeartbeatError>> + Send;
}

/// A failure talking to the licence server. None of these tighten the machine —
/// the caller keeps the last-good lease and lets it age (never off air).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HeartbeatError {
    /// A transport-level failure (connection, TLS, timeout, non-success status).
    #[error("licence-server transport error: {0}")]
    Transport(String),
    /// The server response could not be parsed.
    #[error("malformed licence-server response: {0}")]
    Malformed(String),
    /// The key-trust chain failed to verify (fail closed on trust).
    #[error("key trust failed: {0}")]
    KeyTrust(#[from] KeyTrustError),
    /// The returned signed lease failed to verify.
    #[error("signed lease verification failed: {0}")]
    SignedLease(#[from] SignedLeaseError),
    /// The signed lease is already expired (its cryptographically-signed
    /// `not_after` is at or before now) — a stale or replayed signed lease is
    /// rejected, never installed as a fresh term. The last-good lease is kept.
    #[error("signed lease is already expired (not_after in the past); keeping last-good")]
    LeaseExpired,
    /// The signed lease's `instance_binding_id` does not match this device's
    /// established binding — a valid lease minted for ANOTHER device cannot be
    /// replayed onto this one (cross-instance replay defence). The last-good lease
    /// is kept; nothing is installed.
    #[error("signed lease binds a different instance; refusing cross-instance install")]
    BindingMismatch,
    /// The device's salted hardware fingerprint does not match the lease's machine
    /// (the store's fingerprint-continuity gate rejected the install). The
    /// last-good lease is kept; nothing is installed.
    #[error("device fingerprint does not match; keeping last-good")]
    FingerprintMismatch,
}

/// The device identity material the requests carry — salted digests + opaque ids
/// only (data minimisation, brief §8). The cli assembles this from the
/// fingerprint subsystem; this crate never gathers raw identifiers.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// The registered machine id.
    pub machine_id: String,
    /// The instance id (seat-consuming, lease-bearing unit).
    pub instance_id: String,
    /// The instance binding id, once known (from a prior activation/lease).
    pub binding_id: Option<String>,
    /// The salted hardware-fingerprint digest (lower-case hex SHA-256).
    pub fingerprint_digest: String,
    /// The weighted fingerprint score (0–100).
    pub fingerprint_score: u8,
    /// The salted hardware digest (sibling-instance grouping).
    pub hardware_digest: String,
    /// The instance discriminator hash.
    pub instance_discriminator_hash: String,
    /// The instance discriminator digest (lower-case hex SHA-256).
    pub instance_discriminator_digest: String,
    /// The engine app version.
    pub app_version: String,
    /// The device Ed25519 proof-of-possession public key, base64url (captured +
    /// stored; not used to authenticate requests yet — ADR-0096 D2).
    pub device_public_key_b64url: String,
}

/// The heartbeat-client configuration.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// The organisation id the device activates/heartbeats against. **Config
    /// driven** (ADR-0096 O4): the paid/claim path is fully clear; the free
    /// auto-issue default org is an external-doc residual, so the cli exposes
    /// this as a named config field with a clearly-named placeholder default
    /// rather than a hard-coded guess.
    pub org_id: String,
    /// The optional 6-char claim code (paid order). `None` ⇒ free auto-issue.
    pub claim_code: Option<String>,
    /// The transport label the heartbeat reports (`direct` for a phone-home).
    pub transport: String,
    /// The minimum sleep between contacts when the server does not dictate a
    /// `nextDue` (or on the backoff floor).
    pub min_interval: Duration,
    /// The backoff cap after repeated failures.
    pub max_backoff: Duration,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            // A clearly-named placeholder (NOT a guessed real org id): the cli
            // surfaces this as a config field; an unset value means "no free
            // default configured" (ADR-0096 O4).
            org_id: "org-unset".to_owned(),
            claim_code: None,
            transport: "direct".to_owned(),
            min_interval: Duration::from_secs(60),
            max_backoff: Duration::from_secs(3600),
        }
    }
}

/// The outcome of one heartbeat cycle (for tests + logging).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeartbeatOutcome {
    /// A verified lease was installed; carries the lease serial and `nextDue`.
    Installed {
        /// The installed lease serial.
        serial: String,
        /// When the next heartbeat is due (epoch milliseconds).
        next_due: i64,
    },
    /// The server withheld the lease (`lease: null`, revocation by non-reissue):
    /// the last-good lease is kept and ages naturally. Carries the rung + nextDue.
    LeaseWithheld {
        /// The enforcement rung the server reported.
        state: EnforcementState,
        /// When the next heartbeat is due (epoch milliseconds).
        next_due: i64,
    },
}

/// A wall-clock seam returning epoch milliseconds. The production default is
/// [`unix_millis_now`]; tests inject a controllable clock to drive time-sensitive
/// key-trust re-evaluation (signer validity-window / revocation at lease-acceptance
/// time) deterministically.
pub type NowMs = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The result of an [`HeartbeatClient::install`] attempt — distinguishes a GENUINE
/// install (the lease entered the store) from the benign STALE no-op (the store
/// already held a newer lease and kept it). The caller learns the device binding
/// id ONLY on a genuine install; a stale no-op installed nothing, so learning its
/// binding would poison the device identity with a non-installed (possibly foreign)
/// lease.
enum InstallOutcome {
    /// The verified lease was installed into the store.
    Installed {
        /// The installed lease serial.
        serial: String,
    },
    /// The store already held a newer lease (a benign no-op — never off air).
    /// Nothing was installed; the binding id is NOT learned from this.
    StaleNoop {
        /// The (not-installed) incoming lease serial.
        serial: String,
    },
}

/// The device heartbeat client: drives the verified-lease install convergence and
/// nothing else. Holds only the server handle, the shared lease store, the pinned
/// root, config, and identity — **no engine handle** (invariant #10).
pub struct HeartbeatClient<S: LicenceServer> {
    server: Arc<S>,
    store: Arc<LeaseStore>,
    pinned: PinnedRoot,
    config: HeartbeatConfig,
    identity: DeviceIdentity,
    /// The server-issued `instanceBindingId` learned from a verified lease body
    /// (or the configured identity). Renewals address the binding by THIS id —
    /// **never** the lease serial (a different object). Control-plane only; the
    /// loop is the sole writer/reader, so a plain `Mutex` is correct (no hot path).
    learned_binding_id: std::sync::Mutex<Option<String>>,
    /// The wall-clock seam (epoch ms). Read FRESH at key-trust evaluation AND
    /// again at lease-acceptance, so a signer that expires (or is revoked) during
    /// an arbitrarily-stalled network call is rejected at acceptance (no TOCTOU).
    now_ms: NowMs,
    /// The retry-stable `Idempotency-Key` state for the CURRENT logical operation.
    /// A key is minted ONCE per logical operation and reused on every retry; it
    /// rotates ONLY after a successful contact (install / stale-no-op / withheld),
    /// so a lost-response retry replays the SAME key (the server dedupes — never a
    /// duplicate binding/lease) while a genuinely-new operation gets a fresh key.
    /// Derived from a monotonic per-client counter + the device identity — NEVER
    /// the wall clock (a fresh-per-call wall-clock key defeats dedup). Control-plane
    /// only; the loop is the sole accessor, so a plain `Mutex` is correct.
    idempotency: std::sync::Mutex<IdempotencyState>,
}

/// The retry-stable idempotency-key state for [`HeartbeatClient`]. `counter` only
/// advances when a fresh key is minted (i.e. after a success rotates `current` to
/// `None`), so each logical operation owns one stable key across its retries.
#[derive(Debug, Default)]
struct IdempotencyState {
    /// Monotonic per-client mint counter (advances once per logical operation).
    counter: u64,
    /// The key for the in-flight logical operation, or `None` before the first
    /// mint and after a success rotates it.
    current: Option<String>,
}

impl<S: LicenceServer> HeartbeatClient<S> {
    /// Assemble a heartbeat client with the production wall clock
    /// ([`unix_millis_now`]).
    #[must_use]
    pub fn new(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
    ) -> Self {
        Self::with_clock(
            server,
            store,
            pinned,
            config,
            identity,
            Arc::new(unix_millis_now),
        )
    }

    /// Assemble a heartbeat client reading "now" (epoch ms) from `now_ms` — tests
    /// inject a controllable clock to exercise the key-trust re-evaluation at
    /// lease-acceptance time (the validity-window / revocation TOCTOU).
    #[must_use]
    pub fn with_clock(
        server: Arc<S>,
        store: Arc<LeaseStore>,
        pinned: PinnedRoot,
        config: HeartbeatConfig,
        identity: DeviceIdentity,
        now_ms: NowMs,
    ) -> Self {
        let learned_binding_id = std::sync::Mutex::new(identity.binding_id.clone());
        Self {
            server,
            store,
            pinned,
            config,
            identity,
            learned_binding_id,
            now_ms,
            idempotency: std::sync::Mutex::new(IdempotencyState::default()),
        }
    }

    /// The `Idempotency-Key` for the CURRENT logical operation — minted once and
    /// REPLAYED on every retry until a success rotates it. Derived from a
    /// monotonic per-client counter + the device machine id (stable, never the
    /// wall clock), so a lost-response retry carries the SAME key (the server
    /// dedupes) while a genuinely-new operation gets a distinct key. A poisoned
    /// lock recovers by minting a fresh key (never a panic).
    fn idempotency_key(&self) -> String {
        let mut guard = match self.idempotency.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(existing) = guard.current.clone() {
            return existing;
        }
        let counter = guard.counter.wrapping_add(1);
        guard.counter = counter;
        let key = format!("mv-{}-{counter}", self.identity.machine_id);
        guard.current = Some(key.clone());
        key
    }

    /// Rotate the idempotency key after a SUCCESSFUL contact, so the NEXT logical
    /// operation mints a fresh key. Called only on a positive outcome (install /
    /// stale-no-op / withheld lease) — never on a failure, so a failure's retry
    /// replays the same key. A poisoned lock recovers by clearing the inner value.
    fn rotate_idempotency(&self) {
        match self.idempotency.lock() {
            Ok(mut g) => g.current = None,
            Err(poisoned) => poisoned.into_inner().current = None,
        }
    }

    /// The binding id to address renewals by: the learned/configured
    /// `instanceBindingId`, or `None` until activation discovers it. NEVER the
    /// lease serial.
    fn binding_id(&self) -> Option<String> {
        self.learned_binding_id.lock().ok().and_then(|g| g.clone())
    }

    /// Record the server-issued `instanceBindingId` from a verified lease body so
    /// the next renewal addresses the binding by id.
    fn remember_binding_id(&self, binding_id: &str) {
        if let Ok(mut g) = self.learned_binding_id.lock() {
            *g = Some(binding_id.to_owned());
        }
    }

    /// Build the minimal heartbeat request from the identity (data minimisation:
    /// only the binding id, lease serial, salted digest, app version, transport).
    /// `lease_serial` is the head of the device's current lease chain, if any.
    #[must_use]
    pub fn build_heartbeat_request(
        identity: &DeviceIdentity,
        lease_serial: Option<String>,
    ) -> HeartbeatRequest {
        HeartbeatRequest {
            binding_id: identity.binding_id.clone().unwrap_or_default(),
            lease_serial,
            fingerprint_digest: identity.fingerprint_digest.clone(),
            app_version: identity.app_version.clone(),
            transport: "direct".to_owned(),
        }
    }

    /// Run one heartbeat cycle: fetch + verify the key-trust chain, send a
    /// heartbeat (or activate if no binding is held yet), verify the returned
    /// signed lease, and on success drive `store.install_binding`. On any failure
    /// the store is left untouched (never off air).
    ///
    /// # Errors
    /// [`HeartbeatError`] on transport / trust / verification failure. The caller
    /// (the loop) treats every error as "keep last-good and back off".
    pub async fn run_once(&self) -> Result<HeartbeatOutcome, HeartbeatError> {
        // 1. Fetch + verify the key-trust chain (fail closed on trust). This is a
        //    pre-network fast-fail only; trust is RE-EVALUATED at lease-acceptance
        //    time (step 4) against a FRESHLY RE-FETCHED key document, so a signer
        //    that is revoked OR whose validity window elapses DURING a stalled call
        //    cannot validate the returned lease (no TOCTOU).
        let keys = self.server.fetch_keys().await?;
        let now_ms = (self.now_ms)();
        let _ = TrustedKeys::verify(&keys, &self.pinned, now_ms)?;

        // 2. Heartbeat (or activate when no binding is known yet). The binding is
        //    addressed by the server-issued instanceBindingId — NEVER the lease
        //    serial (a different object). `established_binding` is this device's
        //    locally-anchored identity: the configured/learned heartbeat binding,
        //    OR — when neither is set but a local lease already exists — the
        //    store's current lease binding. So a device that already holds a lease
        //    is NEVER "fresh": a foreign-binding lease is rejected even on the
        //    activate path (it has an identity to violate).
        let held_serial = self.store.current().map(|e| e.lease.serial);
        let established_binding = self
            .binding_id()
            .or_else(|| self.store.current_binding_id());

        // One retry-stable Idempotency-Key for this whole logical operation: the
        // activate/heartbeat mutation replays the SAME key on a retry (the server
        // dedupes — never a duplicate binding/lease on a lost response), and it
        // rotates only after a successful contact below.
        let idempotency_key = self.idempotency_key();
        let (lease, state, next_due) = if established_binding.is_none() {
            // No binding known yet → activate (free auto-issue when no claim code).
            let req = self.build_activate_request();
            let resp = self
                .server
                .activate(&self.config.org_id, req, &idempotency_key)
                .await?;
            (resp.lease, resp.enforcement_state, next_due_default(now_ms))
        } else {
            let req = Self::build_heartbeat_request_for(
                &self.identity,
                held_serial,
                established_binding.clone(),
            );
            let resp = self
                .server
                .heartbeat(&self.config.org_id, req, &idempotency_key)
                .await?;
            (resp.lease, resp.enforcement_state, resp.next_due)
        };

        // 3. A withheld lease (revocation by non-reissue) is a normal outcome:
        //    keep last-good, never tighten. The contact succeeded, so the logical
        //    operation is done — rotate the idempotency key for the next cycle.
        let Some(server_lease) = lease else {
            self.rotate_idempotency();
            return Ok(HeartbeatOutcome::LeaseWithheld { state, next_due });
        };

        // 4. Verify the returned signed lease against the trusted intermediates,
        //    then install it ANCHORED TO THIS DEVICE'S IDENTITY. Only after a
        //    SUCCESSFUL install do we learn the binding id — a rejected
        //    (expired/stale/cross-instance) lease must never mutate the learned
        //    identity (no reject-path poisoning).
        // Re-establish trust at lease ACCEPTANCE time against FRESHLY RE-FETCHED
        // key/revocation material — NOT the pre-network document. Revocation is
        // set-membership over the signed key document, so a fresh clock against the
        // STALE document cannot observe a signer added to the revocation list
        // during an arbitrarily-stalled call; only a re-fetch can. The re-fetched
        // document is itself fully re-verified (`TrustedKeys::verify` re-checks the
        // root match, the root-attested revocation signature, every intermediate's
        // `root_sig`, the signed validity window at the fresh `now()`, and the
        // revocation set), so BOTH a newly-revoked signer AND an elapsed validity
        // window are caught. A signer that is no longer trusted is dropped from the
        // re-fetched `trusted` set, so `verify_signed_lease_chain` cannot resolve
        // `signerKeyId` and rejects the lease (no TOCTOU). The re-fetch fails closed
        // on a transport error (keep last-good, never off air).
        let accept_now_ms = (self.now_ms)();
        let fresh_keys = self.server.fetch_keys().await?;
        let trusted = TrustedKeys::verify(&fresh_keys, &self.pinned, accept_now_ms)?;
        let body = verify_signed_lease_chain(&server_lease, &trusted)?;
        // Install ANCHORED to this device's identity. `remember_binding_id` fires
        // ONLY on a GENUINE install — never on the stale no-op (a Stale outcome
        // means the store kept a newer lease; nothing was installed, so learning a
        // binding from it would poison identity with a non-installed lease). Any
        // install rejection propagates via `?` WITHOUT rotating the idempotency key
        // (the mutation already landed on the server under that key; a retry must
        // replay it so the server dedupes, never mint a fresh key that could
        // duplicate the binding).
        let serial = match self.install(&server_lease, &body, established_binding.as_deref())? {
            InstallOutcome::Installed { serial } => {
                self.remember_binding_id(&body.instance_binding_id);
                serial
            }
            // The store already holds a newer lease — a benign no-op, never off
            // air. Do NOT learn the binding (nothing was installed).
            InstallOutcome::StaleNoop { serial } => serial,
        };
        // The logical operation succeeded end-to-end — rotate the idempotency key
        // so the NEXT cycle is a fresh logical operation.
        self.rotate_idempotency();
        Ok(HeartbeatOutcome::Installed { serial, next_due })
    }

    /// Translate a verified server lease into a [`LeaseBinding`] and drive
    /// [`LeaseStore::install_binding`]. The binding is signed with the crate's
    /// own pinned-key envelope so the single install convergence (shared by the
    /// file-drop and mesh-relay paths) re-verifies it uniformly; the
    /// authoritative Conspect signature was already checked in step 4.
    ///
    /// `established_binding` is this device's locally-anchored instance binding
    /// (configured, or learned from a prior successful install), or `None` before
    /// the first activation. When `Some`, a returned body whose
    /// `instance_binding_id` differs is rejected as a cross-instance replay (a
    /// valid lease minted for another device must not install here); the
    /// fingerprint-strong stamp is only applied to a binding-matched body.
    ///
    /// Returns an [`InstallOutcome`] distinguishing a GENUINE install from the
    /// benign stale no-op (the store already held a newer lease). The caller learns
    /// the binding id ONLY on a genuine install — never on the stale no-op (which
    /// installed nothing, so learning its binding would poison identity).
    ///
    /// # Errors
    /// [`HeartbeatError::BindingMismatch`] for a cross-instance lease;
    /// [`HeartbeatError::LeaseExpired`] for an expired/replayed signed lease;
    /// [`HeartbeatError::SignedLease`] if the local re-verification (which cannot
    /// fail for a body we just signed) rejects it.
    fn install(
        &self,
        server: &ServerLease,
        body: &LeaseBody,
        established_binding: Option<&str>,
    ) -> Result<InstallOutcome, HeartbeatError> {
        // CROSS-INSTANCE REPLAY DEFENCE: once this device has an established
        // binding, a returned lease MUST bind that same instance. A valid
        // Conspect-signed lease minted for another device's binding is refused
        // here (and never reaches the fingerprint-strong stamp below). Before the
        // first activation (`None`) the body's binding is what establishes us.
        if let Some(local) = established_binding {
            if body.instance_binding_id != local {
                return Err(HeartbeatError::BindingMismatch);
            }
        }
        let granted_at = system_now();
        // The installed lease's expiry IS the cryptographically-signed `not_after`
        // (NOT system_now()+35d): a short-lived or replayed-old signed lease must
        // never become a fresh 35-day term. An already-expired signed lease is
        // rejected (keep last-good, never off air).
        let now_ms = (self.now_ms)();
        let lease = Lease::new_online_expiring_at(
            body.serial.clone(),
            body.not_after,
            now_ms,
            ACTIVATION_WINDOW_DAYS,
        )
        .ok_or(HeartbeatError::LeaseExpired)?;
        let gpu_limit = body
            .gpu_limit
            .map_or(GpuLimit::Unlimited, GpuLimit::Limited);
        let hardware = parse_hardware_class(body.hardware_class.as_deref());
        let entitlement = Entitlement::new(
            Tier::new(body.licence_id.clone()),
            hardware,
            hardware,
            gpu_limit,
            lease,
            EntitlementFlags::default(),
        );
        // Re-sign the install envelope with an ephemeral key the store verifies
        // against the matching pinned key. The Conspect signature is already
        // verified; this envelope is the crate's internal install contract. Stamp
        // the device's ACTUAL local fingerprint score (NOT an unconditional
        // STRONG): the store's fingerprint-continuity gate then does real work —
        // a machine whose salted fingerprint does not match (score below the
        // threshold) is rejected rather than silently installed.
        let (binding, install_pinned) = seal_for_install(
            &entitlement,
            self.identity.fingerprint_score,
            &body.instance_binding_id,
        );
        match self
            .store
            .install_binding(&binding, &install_pinned, granted_at)
        {
            Ok(installed) => {
                let _ = server;
                // The store recorded the device's instance binding id ATOMICALLY
                // inside `install_binding` (the single binding-anchor chokepoint
                // every install surface converges on), so the activate-path anchor
                // reads it back without a second write here. Recording happens ONLY
                // on a genuine install — the stale no-op below installs nothing.
                Ok(InstallOutcome::Installed {
                    serial: installed.serial,
                })
            }
            // A stale grant means the store already holds a NEWER lease — a benign
            // no-op, never off air. This is NOT an install: the caller must NOT
            // learn the binding from it (the Stale->Ok fold was the round-3 poison).
            Err(crate::store::InstallError::Stale { .. }) => Ok(InstallOutcome::StaleNoop {
                serial: body.serial.clone(),
            }),
            // The device's fingerprint did not clear the store's continuity gate
            // (the score we stamped is below threshold) — a real keep-last-good
            // outcome now that the score is the device's actual one, not a
            // hardcoded STRONG.
            Err(crate::store::InstallError::FingerprintMismatch { .. }) => {
                Err(HeartbeatError::FingerprintMismatch)
            }
            // The envelope signature cannot fail for a binding we just sealed with
            // the matching pinned key; surface as a verification error, not a panic.
            Err(crate::store::InstallError::SignatureInvalid) => {
                Err(HeartbeatError::SignedLease(SignedLeaseError::BadSignature))
            }
        }
    }

    /// Run the heartbeat loop forever: sleep to the server-dictated `nextDue` (or
    /// the backoff floor), run a cycle, repeat. Best-effort and cancellation-safe
    /// — abort it at any await point and the store's last-good lease is untouched.
    pub async fn run_forever(&self) {
        let mut backoff = self.config.min_interval;
        loop {
            match self.run_once().await {
                Ok(
                    HeartbeatOutcome::Installed { next_due, .. }
                    | HeartbeatOutcome::LeaseWithheld { next_due, .. },
                ) => {
                    backoff = self.config.min_interval;
                    let sleep = sleep_until_due(next_due, self.config.min_interval);
                    tokio::time::sleep(sleep).await;
                }
                Err(err) => {
                    tracing::info!(
                        %err,
                        "heartbeat cycle failed — keeping last-good lease, backing off (never off air)"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.config.max_backoff);
                }
            }
        }
    }

    fn build_activate_request(&self) -> ActivateRequest {
        ActivateRequest {
            machine_id: self.identity.machine_id.clone(),
            claim_code: self.config.claim_code.clone(),
            fingerprint_digest: self.identity.fingerprint_digest.clone(),
            fingerprint_score: self.identity.fingerprint_score,
            hardware_digest: self.identity.hardware_digest.clone(),
            instance_id: self.identity.instance_id.clone(),
            instance_discriminator_hash: self.identity.instance_discriminator_hash.clone(),
            instance_discriminator_digest: self.identity.instance_discriminator_digest.clone(),
            device_public_key: self.identity.device_public_key_b64url.clone(),
            // The server issues + binds the nonce; an empty placeholder until the
            // activation flow threads a server-issued nonce through (the field is
            // required by the wire shape).
            server_nonce: String::new(),
        }
    }

    fn build_heartbeat_request_for(
        identity: &DeviceIdentity,
        held_serial: Option<String>,
        binding_id: Option<String>,
    ) -> HeartbeatRequest {
        HeartbeatRequest {
            binding_id: binding_id.unwrap_or_default(),
            lease_serial: held_serial,
            fingerprint_digest: identity.fingerprint_digest.clone(),
            app_version: identity.app_version.clone(),
            transport: "direct".to_owned(),
        }
    }
}

/// Decode a base64url (no-pad, tolerant of padding) string to bytes; `None` on
/// any non-base64url input (total, panic-free).
fn b64url(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .ok()
}

/// Map a hardware-class string from the signed body onto the typed class. An
/// unknown/absent class defaults to `Standard` (the conservative, broadest tier).
fn parse_hardware_class(s: Option<&str>) -> HardwareClass {
    match s {
        Some("datacenter") => HardwareClass::Datacenter,
        Some("edge") => HardwareClass::Edge,
        _ => HardwareClass::Standard,
    }
}

/// "Now" in epoch milliseconds, from the host wall clock. Off the engine hot loop
/// (the heartbeat task is control-plane). A pre-epoch clock saturates to 0 (the
/// trust check then treats all keys as not-yet-valid — fail closed, never a
/// crash, and never an unwarranted trust).
fn unix_millis_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// The default next-due (epoch ms) when the server does not return one (activate
/// path): `now + 30 days`.
fn next_due_default(now_ms: i64) -> i64 {
    now_ms.saturating_add(30 * 86_400_000)
}

/// How long to sleep until `next_due` (epoch ms), floored at `min` so a stale /
/// past due never spins.
fn sleep_until_due(next_due: i64, min: Duration) -> Duration {
    let now = unix_millis_now();
    let delta_ms = next_due.saturating_sub(now);
    if delta_ms <= 0 {
        return min;
    }
    let secs = u64::try_from(delta_ms / 1000).unwrap_or(0);
    Duration::from_secs(secs).max(min)
}

/// Seal a verified entitlement into the crate's own [`LeaseBinding`] +
/// [`PinnedKey`] install envelope. The Conspect signature is already verified;
/// this envelope is the internal contract the single [`LeaseStore::install_binding`]
/// convergence re-checks uniformly across all producers (file-drop, mesh relay,
/// heartbeat). The signing key is ephemeral and per-call — it never leaves this
/// function and authenticates nothing externally.
fn seal_for_install(
    entitlement: &Entitlement,
    fingerprint_score: u8,
    instance_binding_id: &str,
) -> (LeaseBinding, PinnedKey) {
    use ed25519_dalek::{Signer as _, SigningKey};
    // A deterministic per-process signer derived from nothing secret — its only
    // job is to satisfy the store's internal re-verification of a binding this
    // task itself produced (the authoritative external trust was checked above).
    // It is NOT an identity key and grants no authority.
    let envelope_signer = SigningKey::from_bytes(&INSTALL_ENVELOPE_SEED);
    let pinned = PinnedKey::from_verifying_key(&envelope_signer.verifying_key());
    // Sign over the lease BOUND to the binding id, so the store's re-verification
    // covers the anchor the binding carries (a grafted id would not verify).
    let msg = SignedLease::signing_bytes(&entitlement.lease, Some(instance_binding_id));
    let sig = envelope_signer.sign(&msg);
    let signed_lease = SignedLease::new(entitlement.lease.clone(), sig.to_bytes());
    // The device's ACTUAL local fingerprint score — NOT an unconditional STRONG.
    // The store's fingerprint-continuity gate then genuinely rejects a machine
    // whose salted fingerprint does not match (score below threshold). The
    // verified binding id rides along so `install_binding` records the device's
    // instance identity atomically (the single binding-anchor chokepoint).
    let binding = LeaseBinding::new(
        signed_lease,
        entitlement.clone(),
        fingerprint_score,
        Some(instance_binding_id.to_owned()),
    );
    (binding, pinned)
}

/// A fixed seed for the ephemeral install-envelope signer (see
/// [`seal_for_install`]). Not a secret and not an identity — it only closes the
/// store's internal binding re-verification for a binding this task produced.
const INSTALL_ENVELOPE_SEED: [u8; 32] = [0x6d; 32];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_key_cmp_orders_shortest_then_bytewise() {
        // Shorter encoded key sorts first (RFC 8949 §4.2.1): "serial" (6) before
        // "not_after" (9).
        assert_eq!(canonical_key_cmp("a", "bb"), std::cmp::Ordering::Less);
        assert_eq!(
            canonical_key_cmp("serial", "not_after"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            canonical_key_cmp("not_after", "serial"),
            std::cmp::Ordering::Greater
        );
        // Equal length → bytewise: "licence_id" vs "not_after_x" differ in length
        // so length wins; same-length keys fall back to bytewise order.
        assert_eq!(canonical_key_cmp("aaa", "aab"), std::cmp::Ordering::Less);
        assert_eq!(
            canonical_key_cmp("gpu_limit", "gpu_limit"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn cbor_head_uses_shortest_encoding() {
        let mut a = Vec::new();
        cbor_head(&mut a, 0, 10);
        assert_eq!(a, vec![0x0a]);
        let mut b = Vec::new();
        cbor_head(&mut b, 0, 1_000_000);
        assert_eq!(b[0], 0x1a); // 4-byte argument
    }

    #[test]
    fn b64url_round_trips_and_rejects_garbage() {
        assert!(b64url("aGVsbG8").is_some());
        assert!(b64url("not base64!").is_none());
    }
}
