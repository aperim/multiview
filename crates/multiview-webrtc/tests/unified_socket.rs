//! Failing-first tests for the single-socket [`UnifiedEndpoint`] (ADR-0048 §4,
//! box-validation defect B). The cli previously bound `webrtc.udp_port` once per
//! role (preview WHEP + WHIP ingest + WHEP-serve + each whip_push), so the 2nd/3rd
//! `bind` hit `EADDRINUSE` and silently degraded those roles to "unavailable" —
//! with preview + WHIP + WHEP-serve in one config, ingest + output-serve were dead.
//!
//! The fix: ONE bound dual-stack socket adopted by ALL roles. These tests prove a
//! single [`UnifiedEndpoint`] hosts preview + WHIP + WHEP-serve at once, each role
//! reachable (negotiates an answer) — no second bind, no degrade.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

use std::net::SocketAddr;
use std::time::Instant;

use multiview_webrtc::config::EndpointConfig;
use multiview_webrtc::transport::{MediaKind, Session, SessionConfig, UnifiedEndpoint};

/// Build a publisher/viewer offer (a browser/OBS-shaped offer with a host
/// candidate) so each role has a real SDP to negotiate against.
fn offer(direction_recv: bool) -> String {
    let now = Instant::now();
    let mut s = Session::new(&SessionConfig::default(), now);
    s.add_host_candidate("[::1]:55000".parse::<SocketAddr>().unwrap())
        .unwrap();
    if direction_recv {
        s.create_recv_offer(&[MediaKind::Video, MediaKind::Audio])
            .unwrap()
    } else {
        s.create_offer(&[MediaKind::Video, MediaKind::Audio]).unwrap()
    }
}

#[test]
fn one_socket_hosts_preview_whip_and_whep_serve_without_eaddrinuse() {
    // The whole point of defect B: preview + WHIP ingest + WHEP-serve coexist on
    // ONE bound socket. Build a single endpoint, register all three roles, and
    // confirm each negotiates an answer — i.e. none silently degraded because a
    // second bind clashed.
    let cfg = EndpointConfig {
        // Ephemeral port (bind is local); a concrete advertised addr so the
        // gathered candidate is valid (the unspecified bind addr is not).
        udp_port: 0,
        advertised_addresses: vec!["::1".parse().unwrap()],
        ..EndpointConfig::default()
    };

    // ONE bind for the whole endpoint.
    let builder = UnifiedEndpoint::bind(cfg).expect("the single shared socket binds once");

    // Register WHIP ingest + WHEP-serve, and the native preview egress — all on the
    // one socket. The preview egress wants a CONCRETE host candidate (the
    // unspecified bind addr is not a valid str0m candidate), matching how the cli
    // wires it.
    let host = *builder
        .host_candidates()
        .iter()
        .find(|a| !a.ip().is_unspecified())
        .expect("a concrete advertised host candidate");
    let (builder, whip) = builder.with_ingest();
    let (builder, whep_serve) = builder.with_serve();
    let preview = std::sync::Arc::new(
        multiview_webrtc::whep_egress::WhepEgress::with_host_candidate(host),
    );
    let _endpoint = builder.with_preview(std::sync::Arc::clone(&preview)).build();

    // 1. WHIP ingest negotiates a publisher answer.
    let whip_answer = whip
        .negotiate("cam-1", &offer(false), true)
        .expect("WHIP ingest is reachable on the shared socket (not EADDRINUSE-dead)");
    assert!(
        whip_answer.answer_sdp.contains("a=group:BUNDLE"),
        "WHIP answered str0m's own SDP"
    );

    // 2. WHEP-serve negotiates a viewer answer for a registered output.
    whep_serve.register_output("prog-out", 8, test_feed());
    let whep_answer = whep_serve
        .negotiate("prog-out", &offer(true), true)
        .expect("WHEP-serve is reachable on the shared socket (not EADDRINUSE-dead)");
    assert!(
        whep_answer.answer_sdp.contains("a=group:BUNDLE"),
        "WHEP-serve answered str0m's own SDP"
    );

    // 3. Preview WHEP egress negotiates a viewer answer.
    let preview_answer = preview
        .accept_session(
            &offer(true),
            multiview_preview::whep::PreviewCodec::H264,
            &FakeMedia::default(),
        )
        .expect("preview WHEP is reachable on the shared socket (not EADDRINUSE-dead)");
    assert!(
        preview_answer.sdp_answer.contains("a=group:BUNDLE"),
        "preview answered str0m's own SDP"
    );

    // All three roles negotiated — they share the single socket, none degraded.
    assert_eq!(whip.live_publisher_count(), 1);
    assert_eq!(whep_serve.live_viewer_count("prog-out"), 1);
}

/// A bounded egress feed for the WHEP-serve output registration. The sink is
/// leaked alive for the test's duration so the feed stays open.
fn test_feed() -> multiview_webrtc::egress::EgressFeed {
    let (sink, feed) = multiview_webrtc::egress::egress_feed();
    std::mem::forget(sink);
    feed
}

/// A minimal `PreviewMediaSource` for the preview negotiation (no real media; the
/// test only exercises negotiation reachability on the shared socket).
#[derive(Default)]
struct FakeMedia;

impl multiview_preview::whep::transport::PreviewMediaSource for FakeMedia {
    fn codec(&self) -> multiview_preview::whep::PreviewCodec {
        multiview_preview::whep::PreviewCodec::H264
    }
    fn feed(&self) -> multiview_preview::whep::transport::SampleFeed {
        let (sink, feed) = multiview_preview::whep::transport::sample_feed(8);
        std::mem::forget(sink);
        feed
    }
    fn audio_feed(&self) -> Option<multiview_preview::whep::transport::SampleFeed> {
        None
    }
}
