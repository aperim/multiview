//! TLS-0 (ADR-W029): the `[control.tls]` static-cert schema parses, is
//! `mode`-tagged (never `untagged`), and validates cert/key paths fail-closed.
//!
//! The runtime rustls termination lives in `multiview-control`; this crate only
//! models + validates the knobs (mirroring the `[control.limits]` split). These
//! tests pin the wire shape the operator writes and the fail-closed validation
//! `MultiviewConfig::validate` enforces at config load.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{ControlConfig, TlsConfig};

/// A `[control.tls]` table with `mode = "static"` + cert/key deserializes to the
/// `Static` variant with both paths, carried on `ControlConfig::tls`.
#[test]
fn static_mode_parses_with_cert_and_key() {
    let control: ControlConfig = serde_json::from_str(
        r#"{
            "listen": "[::1]:8080",
            "tls": { "mode": "static", "cert": "/etc/mv/cert.pem", "key": "/etc/mv/key.pem" }
        }"#,
    )
    .expect("a static-mode tls table must deserialize");

    match control.tls {
        Some(TlsConfig::Static { cert, key }) => {
            assert_eq!(cert, std::path::PathBuf::from("/etc/mv/cert.pem"));
            assert_eq!(key, std::path::PathBuf::from("/etc/mv/key.pem"));
        }
        other => panic!("expected Some(TlsConfig::Static {{ .. }}), got {other:?}"),
    }
}

/// A `[control]` section with no `[control.tls]` leaves `tls` absent — the
/// default plain-HTTP posture (no native TLS dep, no cert required).
#[test]
fn absent_tls_is_none() {
    let control: ControlConfig =
        serde_json::from_str(r#"{ "listen": "[::]:8080" }"#).expect("a control without tls parses");
    assert!(
        control.tls.is_none(),
        "no [control.tls] ⇒ plain HTTP (tls == None)"
    );
}

/// The union is `mode`-tagged, not `untagged`: an unknown `mode` fails to parse
/// (validated by construction — a typo can never silently pick a variant).
#[test]
fn unknown_mode_fails_to_parse() {
    let parsed: Result<TlsConfig, _> =
        serde_json::from_str(r#"{ "mode": "acme-dns01", "cert": "/c", "key": "/k" }"#);
    assert!(
        parsed.is_err(),
        "an unknown tls mode must fail to parse (mode-tagged, never untagged)"
    );
}

/// `static` mode requires BOTH a cert and a key: a missing field fails to parse
/// (no all-optional silent-empty config).
#[test]
fn static_mode_requires_cert_and_key() {
    let no_key: Result<TlsConfig, _> =
        serde_json::from_str(r#"{ "mode": "static", "cert": "/c" }"#);
    assert!(no_key.is_err(), "static mode without a key must fail to parse");
    let no_cert: Result<TlsConfig, _> =
        serde_json::from_str(r#"{ "mode": "static", "key": "/k" }"#);
    assert!(no_cert.is_err(), "static mode without a cert must fail to parse");
}

/// An empty cert path is rejected by validation (fail-closed at config load —
/// an empty path would never load a certificate).
#[test]
fn empty_cert_path_fails_validation() {
    let tls = TlsConfig::Static {
        cert: std::path::PathBuf::new(),
        key: std::path::PathBuf::from("/etc/mv/key.pem"),
    };
    assert!(
        tls.validate().is_err(),
        "an empty cert path must fail config-load validation"
    );
}

/// An empty key path is rejected by validation (fail-closed at config load).
#[test]
fn empty_key_path_fails_validation() {
    let tls = TlsConfig::Static {
        cert: std::path::PathBuf::from("/etc/mv/cert.pem"),
        key: std::path::PathBuf::new(),
    };
    assert!(
        tls.validate().is_err(),
        "an empty key path must fail config-load validation"
    );
}

/// A fully-specified static config validates OK — the happy path is accepted.
#[test]
fn a_complete_static_config_validates() {
    let tls = TlsConfig::Static {
        cert: std::path::PathBuf::from("/etc/mv/cert.pem"),
        key: std::path::PathBuf::from("/etc/mv/key.pem"),
    };
    tls.validate()
        .expect("a static config with both paths present must validate");
}
