//! Failing-first tests for the honest IPv6-first SDP helpers (ADR-0048 §10,
//! ADR-0042). The fake-path answer must use `c=IN IP6 ::` (never `IP4 0.0.0.0`)
//! and carry `a=mid` / `a=rtcp-mux`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use multiview_webrtc::sdp::{candidate_priority_order, AnswerParams, CandidateClass};

#[test]
fn fake_answer_is_ipv6_first_and_has_mid_and_rtcp_mux() {
    let answer = AnswerParams {
        ice_ufrag: "ufrag1".to_owned(),
        ice_pwd: "pwd123456789012345678".to_owned(),
        fingerprint_algorithm: "sha-256".to_owned(),
        fingerprint_value: "AA:BB:CC".to_owned(),
        video_payload_type: 96,
        video_codec: "H264".to_owned(),
        audio_payload_type: Some(111),
        audio_codec: Some("opus".to_owned()),
    }
    .build_sdp();

    // IPv6-first: connection line is `c=IN IP6 ::`, NEVER `c=IN IP4 0.0.0.0`.
    assert!(answer.contains("c=IN IP6 ::"), "answer:\n{answer}");
    assert!(!answer.contains("IP4 0.0.0.0"), "no IPv4 placeholder");
    assert!(!answer.contains("0.0.0.0"));
    // Origin is IPv6 too.
    assert!(answer.contains("o=") && answer.contains("IN IP6 ::"));
    // BUNDLE + mid + rtcp-mux present.
    assert!(answer.contains("a=group:BUNDLE"));
    assert!(answer.contains("a=mid:"));
    assert!(answer.contains("a=rtcp-mux"));
    // The video codec is advertised at its payload type.
    assert!(answer.contains("a=rtpmap:96 H264/90000"));
    // Opus audio at 48 kHz / 2 channels.
    assert!(answer.contains("a=rtpmap:111 opus/48000/2"));
    // It is the answerer, so it is the DTLS server: setup:passive.
    assert!(answer.contains("a=setup:passive"));
}

#[test]
fn audioless_answer_omits_the_audio_mline() {
    let answer = AnswerParams {
        ice_ufrag: "u".to_owned(),
        ice_pwd: "p2345678901234567890".to_owned(),
        fingerprint_algorithm: "sha-256".to_owned(),
        fingerprint_value: "AA".to_owned(),
        video_payload_type: 96,
        video_codec: "H264".to_owned(),
        audio_payload_type: None,
        audio_codec: None,
    }
    .build_sdp();
    assert!(answer.contains("m=video"));
    assert!(!answer.contains("m=audio"));
}

#[test]
fn candidate_priority_orders_ipv6_before_ipv4_and_host_before_relay() {
    // ADR-0042 / ADR-0048: IPv6 leads; relay candidates are lowest priority (the
    // last resort for NAT traversal). The operator's TURN relay still appears —
    // ordering, never omission.
    let ordered = candidate_priority_order(&[
        CandidateClass::HostV4,
        CandidateClass::RelayV6,
        CandidateClass::HostV6,
        CandidateClass::ServerReflexiveV4,
        CandidateClass::RelayV4,
        CandidateClass::ServerReflexiveV6,
    ]);
    // IPv6 host first, IPv4 relay last; every input class is present (none dropped).
    assert_eq!(ordered.first(), Some(&CandidateClass::HostV6));
    assert_eq!(ordered.last(), Some(&CandidateClass::RelayV4));
    assert_eq!(
        ordered.len(),
        6,
        "TURN relay candidates are ordered, never dropped"
    );
    // Within a family, host > srflx > relay; across families v6 > v4 at each tier.
    let pos = |c: CandidateClass| ordered.iter().position(|x| *x == c).unwrap();
    assert!(pos(CandidateClass::HostV6) < pos(CandidateClass::HostV4));
    assert!(pos(CandidateClass::HostV4) < pos(CandidateClass::ServerReflexiveV6));
    assert!(pos(CandidateClass::ServerReflexiveV4) < pos(CandidateClass::RelayV6));
    assert!(pos(CandidateClass::RelayV6) < pos(CandidateClass::RelayV4));
}
