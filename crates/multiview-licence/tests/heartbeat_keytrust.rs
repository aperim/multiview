//! CONSPECT-3 **key-trust** + signed-lease verification tests (ADR-0096, D1/D3).
//!
//! The key-trust cases verify against the **live production well-known document**
//! (`GET https://api.conspect.studio/.well-known/conspect-licensing-keys.json`,
//! captured 2026-06-16): the real pinned root (ECDSA P-256) must attest the real
//! dual-pin Ed25519 intermediates and the real revocation statement — so the
//! verifier is proven against the authoritative trust material itself, the
//! strongest possible golden vector. The signed-lease cases mint locally-signed
//! leases under a fabricated keyset whose root we control (the production private
//! keys are server-side only).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]
#![cfg(feature = "heartbeat")]

use multiview_licence::heartbeat::{
    canonical_key_preimage, verify_signed_lease_chain, KeyTrustError, LicensingKeys, PinnedRoot,
    SignedLeaseError, TrustedKeys,
};

mod fake;
use fake::{b64url, FabricatedKeyset, ROOT_PUB_B64URL};

/// The current dual-pin intermediate (Ed25519), from the live well-known doc.
const IV1_KID: &str = "intermediate-v1";
const IV1_PUB_B64URL: &str = "EYwdPYlasNl8JRksWWrFbGBrJD4SFumP0SeLDv2R3Cc";
const IV1_VALID_FROM: i64 = 1_781_510_167_548;
const IV1_VALID_UNTIL: i64 = 1_816_070_167_548;
const IV2_KID: &str = "intermediate-v2";
const IV2_VALID_UNTIL: i64 = 1_850_630_167_548;
/// A wall instant inside both intermediates' validity (epoch ms).
const NOW_MS_VALID: i64 = 1_790_000_000_000;
const KEY_STATEMENT: &str = "conspect.key-attestation.v1";

/// The live production well-known document, verbatim.
const LIVE_KEYS_JSON: &str = include_str!("fixtures/conspect-licensing-keys.json");

fn pinned_root() -> PinnedRoot {
    PinnedRoot::from_base64url(ROOT_PUB_B64URL).expect("the production root key must parse")
}

// --- D3: canonical-CBOR key pre-image golden vector --------------------------

#[test]
fn canonical_key_preimage_matches_the_known_golden_bytes() {
    let pre = canonical_key_preimage(
        IV1_KID,
        "lease",
        KEY_STATEMENT,
        &b64url(IV1_PUB_B64URL),
        IV1_VALID_FROM,
        IV1_VALID_UNTIL,
    );
    // Byte-exact RFC 8949 §4.2.1 (cross-checked with an independent Python CBOR
    // encoder + the live root_sig that signs exactly these bytes).
    let golden = "a6666b65795f69646f696e7465726d6564696174652d7631686b65795f74797065656c656173656973746174656d656e74781b636f6e73706563742e6b65792d6174746573746174696f6e2e76316a7075626c69635f6b65795820118c1d3d895ab0d97c25192c596ac56c606b243e1216e98fd1278b0efd91dc276a76616c69645f66726f6d1b0000019eca47dbfc6b76616c69645f756e74696c1b000001a6d6379bfc";
    assert_eq!(hex::encode(&pre), golden);
}

// --- D1: key-trust chain, against the LIVE well-known doc ---------------------

#[test]
fn live_wellknown_intermediates_verify_against_the_pinned_root() {
    let keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).expect("well-known parses");
    let trusted = TrustedKeys::verify(&keys, &pinned_root(), NOW_MS_VALID)
        .expect("the production keyset must verify against the pinned root");
    assert!(
        trusted.lease_key(IV1_KID).is_some(),
        "current intermediate trusted"
    );
    assert!(
        trusted.lease_key(IV2_KID).is_some(),
        "next (dual-pin) intermediate trusted"
    );
    assert!(trusted.lease_key("intermediate-vX").is_none());
}

#[test]
fn an_intermediate_not_attested_by_the_root_is_rejected() {
    let mut keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    // Swap intermediate-v1's public key: its root_sig no longer covers it.
    keys.lease_keys[0].public_key = "ht_UXIIuzGj5Tk2NQr0fg9NlkYab_eKUVtWlNlMLERg".to_owned();
    let err = TrustedKeys::verify(&keys, &pinned_root(), NOW_MS_VALID)
        .expect_err("a non-root-attested intermediate must be rejected");
    assert!(
        matches!(err, KeyTrustError::IntermediateNotAttested { .. }),
        "got {err:?}"
    );
}

#[test]
fn an_intermediate_outside_its_validity_window_is_not_trusted() {
    let keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    let after_all = IV2_VALID_UNTIL + 1;
    let trusted = TrustedKeys::verify(&keys, &pinned_root(), after_all)
        .expect("an expired keyset still root-verifies; keys age out of the trusted set");
    assert!(trusted.lease_key(IV1_KID).is_none());
    assert!(trusted.lease_key(IV2_KID).is_none());
    let _ = IV1_VALID_UNTIL;
}

#[test]
fn a_forged_revocation_signature_is_rejected() {
    let mut keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    keys.revocation.root_revocation_sig = keys.lease_keys[0].root_sig.clone();
    let err = TrustedKeys::verify(&keys, &pinned_root(), NOW_MS_VALID)
        .expect_err("a forged revocation signature must be rejected (fail closed)");
    assert!(
        matches!(err, KeyTrustError::RevocationNotAttested),
        "got {err:?}"
    );
}

#[test]
fn a_wrong_pinned_root_rejects_the_whole_keyset() {
    let keys: LicensingKeys = serde_json::from_str(LIVE_KEYS_JSON).unwrap();
    // A different (fabricated) root cannot have signed the production attestations.
    // The verifier rejects up front with `RootMismatch` (the advertised root in the
    // document does not byte-match the pinned anchor) — a substituted well-known
    // document is caught before any signature is even checked. A keyset that
    // *omitted* the advertised root would instead fail at attestation; either way
    // the foreign-root keyset is rejected, which is the property under test.
    let kit = FabricatedKeyset::new();
    let err = TrustedKeys::verify(&keys, &kit.pinned_root(), NOW_MS_VALID)
        .expect_err("the live keyset must not verify under a foreign root");
    assert!(
        matches!(
            err,
            KeyTrustError::RootMismatch
                | KeyTrustError::IntermediateNotAttested { .. }
                | KeyTrustError::RevocationNotAttested
        ),
        "got {err:?}"
    );
}

// --- Revocation drop (fabricated root we control) ----------------------------

#[test]
fn a_revoked_signer_key_id_is_dropped_from_the_trusted_set() {
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_revocation(&[kit.kid()]);
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms())
        .expect("a keyset whose only key is revoked still verifies structurally");
    assert!(
        trusted.lease_key(kit.kid()).is_none(),
        "a revoked signerKeyId must be dropped from the trusted set"
    );
}

#[test]
fn a_non_revoked_signer_key_id_survives_with_an_unrelated_revocation() {
    let kit = FabricatedKeyset::new();
    let keys = kit.keys_with_revocation(&["intermediate-someone-else"]);
    let trusted = TrustedKeys::verify(&keys, &kit.pinned_root(), kit.now_ms()).unwrap();
    assert!(
        trusted.lease_key(kit.kid()).is_some(),
        "an unrelated revocation must not drop our key"
    );
}

// --- D3: signed-lease (bare Ed25519 over decoded leaseBytes) ------------------

#[test]
fn a_validly_signed_lease_verifies_and_yields_the_lease_body() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let lease = kit.sign_lease(35);
    let body =
        verify_signed_lease_chain(&lease, &trusted).expect("a correctly-signed lease verifies");
    assert_eq!(body.licence_id, kit.licence_id());
    assert!(body.not_after > kit.now_ms());
    assert_eq!(body.gpu_limit, Some(2));
}

#[test]
fn a_lease_with_a_broken_ed25519_signature_is_rejected() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    let mut sig = hex::decode(&lease.signature).unwrap();
    sig[0] ^= 0xff;
    lease.signature = hex::encode(sig);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a tampered signature must be rejected");
    assert!(matches!(err, SignedLeaseError::BadSignature), "got {err:?}");
}

#[test]
fn a_lease_whose_leasebytes_were_tampered_is_rejected() {
    use base64::Engine;
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    let mut raw = base64::engine::general_purpose::STANDARD
        .decode(lease.lease_bytes.trim_end_matches('='))
        .unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 0x01;
    lease.lease_bytes = base64::engine::general_purpose::STANDARD.encode(&raw);
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a tampered lease body must be rejected");
    assert!(matches!(err, SignedLeaseError::BadSignature), "got {err:?}");
}

#[test]
fn a_lease_signed_by_an_unknown_signer_key_id_is_rejected() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    lease.signer_key_id = "intermediate-ghost".to_owned();
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("an unknown signerKeyId must be rejected");
    assert!(
        matches!(err, SignedLeaseError::UnknownSigner { .. }),
        "got {err:?}"
    );
}

#[test]
fn a_lease_with_a_non_hex_signature_is_rejected_not_panicked() {
    let kit = FabricatedKeyset::new();
    let trusted = kit.trusted();
    let mut lease = kit.sign_lease(35);
    lease.signature = "not-hex-zz".to_owned();
    let err = verify_signed_lease_chain(&lease, &trusted)
        .expect_err("a malformed signature must be a typed error, never a panic");
    assert!(
        matches!(err, SignedLeaseError::MalformedSignature),
        "got {err:?}"
    );
}
