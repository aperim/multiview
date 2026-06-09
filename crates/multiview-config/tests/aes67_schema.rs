//! AES67 / ST 2110-30 config-schema tests: the `SourceKind::Aes67` input and the
//! `Output::Aes67` audio-RTP sink (RFC 4566 SDP-bound, IPv6 multicast). Pure
//! serde — TOML/JSON round-trip, no engine, no network.
//!
//! IPv6-first (ADR-0042): AES67 multicast examples lead with an IPv6 group
//! (`[ff3e::1]:5004`), never an IPv4-only literal.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_config::{Output, OutputAudio, OutputAudioMode, Source, SourceKind};

// ---------------------------------------------------------------------------
// SourceKind::Aes67
// ---------------------------------------------------------------------------

#[test]
fn source_aes67_deserializes_static_sdp() {
    let toml_str = r#"
id = "aes67_cam"
kind = "aes67"
sdp = """
m=audio 5004 RTP/AVP 98
a=rtpmap:98 L24/48000/2
a=ptime:1
a=ts-refclk:ptp=IEEE1588-2008:AABBCCDD:0
a=mediaclk:direct=0
"""
multicast = "[ff3e::1]:5004"
link_offset_ms = 100
ptp_domain = 0
"#;
    let src: Source = toml::from_str(toml_str).expect("valid AES67 source");
    assert_eq!(src.id, "aes67_cam");
    match &src.kind {
        SourceKind::Aes67 {
            sdp,
            multicast,
            link_offset_ms,
            ptp_domain,
            session_id,
        } => {
            assert!(sdp.contains("m=audio"));
            assert_eq!(multicast.as_deref(), Some("[ff3e::1]:5004"));
            assert_eq!(*link_offset_ms, Some(100));
            assert_eq!(*ptp_domain, Some(0));
            assert_eq!(*session_id, None);
        }
        other => panic!("expected Aes67, got {other:?}"),
    }
}

#[test]
fn source_aes67_minimal_only_sdp() {
    let toml_str = r#"
id = "minimal"
kind = "aes67"
sdp = "m=audio 5004 RTP/AVP 98"
"#;
    let src: Source = toml::from_str(toml_str).expect("minimal AES67 source");
    match &src.kind {
        SourceKind::Aes67 {
            multicast,
            link_offset_ms,
            ptp_domain,
            session_id,
            ..
        } => {
            assert_eq!(*multicast, None);
            assert_eq!(*link_offset_ms, None);
            assert_eq!(*ptp_domain, None);
            assert_eq!(*session_id, None);
        }
        other => panic!("expected Aes67, got {other:?}"),
    }
}

#[test]
fn source_aes67_roundtrip_toml() {
    let original = Source {
        id: "aes67_1".to_string(),
        display_name: None,
        kind: SourceKind::Aes67 {
            sdp: "m=audio 5004 RTP/AVP 98".to_string(),
            session_id: Some("sap:0x1234".to_string()),
            multicast: Some("[ff3e::1]:5004".to_string()),
            link_offset_ms: Some(100),
            ptp_domain: Some(1),
        },
        auth: None,
        color_override: None,
        captions: None,
        gpu_pin: None,
        wallclock: None,
    };
    let toml_str = toml::to_string(&original).expect("serialize");
    let reparsed: Source = toml::from_str(&toml_str).expect("re-parse");
    assert_eq!(original, reparsed);
}

#[test]
fn source_aes67_roundtrip_json() {
    let original = Source {
        id: "aes67_json".to_string(),
        display_name: Some("Studio Mic".to_string()),
        kind: SourceKind::Aes67 {
            sdp: "m=audio 5004 RTP/AVP 97\na=rtpmap:97 L16/48000/1".to_string(),
            session_id: None,
            multicast: Some("[ff3e::2]:5004".to_string()),
            link_offset_ms: None,
            ptp_domain: Some(0),
        },
        auth: None,
        color_override: None,
        captions: None,
        gpu_pin: None,
        wallclock: None,
    };
    let json = serde_json::to_string(&original).expect("serialize json");
    let reparsed: Source = serde_json::from_str(&json).expect("re-parse json");
    assert_eq!(original, reparsed);
    // Internal tag, not untagged: `kind` is a top-level discriminator.
    assert!(json.contains("\"kind\":\"aes67\""));
}

// ---------------------------------------------------------------------------
// Output::Aes67
// ---------------------------------------------------------------------------

#[test]
fn output_aes67_deserializes() {
    let toml_str = r#"
kind = "aes67"
id = "aes67_out"
label = "Studio AES67"
multicast = "[ff3e::1]:5004"
depth = "L24"
ptime_ms = 1
ptp_domain = 0
"#;
    let out: Output = toml::from_str(toml_str).expect("valid AES67 output");
    match &out {
        Output::Aes67 {
            id,
            label,
            multicast,
            depth,
            ptime_ms,
            ptp_domain,
            audio,
            ..
        } => {
            assert_eq!(id.as_deref(), Some("aes67_out"));
            assert_eq!(label, "Studio AES67");
            assert_eq!(multicast, "[ff3e::1]:5004");
            assert_eq!(depth, "L24");
            assert_eq!(*ptime_ms, 1);
            assert_eq!(*ptp_domain, Some(0));
            assert_eq!(*audio, None);
        }
        other => panic!("expected Aes67 output, got {other:?}"),
    }
}

#[test]
fn output_aes67_defaults_depth_and_ptime() {
    // depth + ptime_ms omitted => Class A defaults (L24, 1 ms).
    let toml_str = r#"
kind = "aes67"
label = "Defaulted"
multicast = "[ff3e::1]:5004"
"#;
    let out: Output = toml::from_str(toml_str).expect("AES67 output with defaults");
    match &out {
        Output::Aes67 {
            depth, ptime_ms, ..
        } => {
            assert_eq!(depth, "L24", "depth defaults to L24 (Class A interop)");
            assert_eq!(*ptime_ms, 1, "ptime defaults to 1 ms (Class A)");
        }
        other => panic!("expected Aes67 output, got {other:?}"),
    }
}

#[test]
fn output_aes67_label_and_gpu_pin_and_audio_accessors() {
    let out = Output::Aes67 {
        id: Some("aes67".to_string()),
        label: "AES67 Out".to_string(),
        multicast: "[ff3e::1]:5004".to_string(),
        depth: "L24".to_string(),
        ptime_ms: 1,
        ptp_domain: Some(0),
        gpu_pin: None,
        audio: Some(OutputAudio {
            mode: OutputAudioMode::Program,
            tracks: Vec::new(),
        }),
    };
    // AES67 is the first output with no encode stage: gpu_pin is always None.
    assert!(out.gpu_pin().is_none());
    // The label() helper returns the carried label for AES67.
    assert_eq!(out.label(), "AES67 Out");
    // explicit_id() surfaces the authored id.
    assert_eq!(out.explicit_id(), Some("aes67"));
    // audio() exposes the program-bus selector.
    assert!(matches!(
        out.audio(),
        Some(OutputAudio {
            mode: OutputAudioMode::Program,
            ..
        })
    ));
}

#[test]
fn output_aes67_roundtrip_json_internal_tag() {
    let original = Output::Aes67 {
        id: None,
        label: "RT AES67".to_string(),
        multicast: "[ff3e::5]:5004".to_string(),
        depth: "L16".to_string(),
        ptime_ms: 1,
        ptp_domain: None,
        gpu_pin: None,
        audio: None,
    };
    let json = serde_json::to_string(&original).expect("serialize");
    let reparsed: Output = serde_json::from_str(&json).expect("re-parse");
    assert_eq!(original, reparsed);
    assert!(json.contains("\"kind\":\"aes67\""));
}
