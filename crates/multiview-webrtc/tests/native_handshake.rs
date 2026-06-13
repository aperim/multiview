//! Failing-first, **offline** tests for the native str0m-backed transport
//! ([`multiview_webrtc::transport`], feature `native`).
//!
//! str0m is sans-IO: it owns no socket. That is the whole win here — two
//! [`Session`]s are driven entirely in memory through a packet shuttle (each
//! `Output::Transmit` from one peer becomes an `Input::Receive` on the other),
//! so a complete ICE + DTLS handshake and SRTP-protected RTP media exchange run
//! with **no network and no real socket** on an ordinary CI runner. The
//! live-network legs (real UDP, real TURN server) are `#[ignore]`d and
//! hardware-gated.
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::missing_panics_doc
)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use multiview_webrtc::transport::{MediaKind, Session, SessionConfig};

/// The offerer/answerer host addresses (loopback-style, but never actually
/// bound — the shuttle moves bytes by value). IPv6-first per ADR-0042.
const OFFERER_ADDR: &str = "[::1]:40001";
const ANSWERER_ADDR: &str = "[::1]:40002";

/// Drive both peers' sans-IO loops, shuttling every datagram one peer emits into
/// the other peer, advancing a shared virtual clock, until `done` returns true or
/// the iteration bound is hit. Returns whether `done` was reached.
fn pump_until(
    a: &mut Session,
    a_addr: SocketAddr,
    b: &mut Session,
    b_addr: SocketAddr,
    mut clock: Instant,
    mut done: impl FnMut(&mut Session, &mut Session) -> bool,
) -> bool {
    for _ in 0..2000 {
        // Feed each peer a timeout tick so its ICE/DTLS timers advance, then
        // drain everything it wants to send into the other peer.
        a.handle_timeout(clock).unwrap();
        b.handle_timeout(clock).unwrap();
        for _ in 0..128 {
            match a.poll_transmit(clock) {
                Some((dst, payload)) => {
                    // The destination should be the other peer's address.
                    assert_eq!(dst, b_addr, "a transmits to b");
                    b.handle_datagram(a_addr, b_addr, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        for _ in 0..128 {
            match b.poll_transmit(clock) {
                Some((dst, payload)) => {
                    assert_eq!(dst, a_addr, "b transmits to a");
                    a.handle_datagram(b_addr, a_addr, &payload, clock).unwrap();
                }
                None => break,
            }
        }
        if done(a, b) {
            return true;
        }
        // Advance the virtual clock past whichever peer wakes soonest so DTLS /
        // ICE timers fire deterministically.
        let next = a.poll_timeout(clock).min(b.poll_timeout(clock));
        clock = next.max(clock + Duration::from_millis(1));
    }
    false
}

fn offerer_answerer() -> (Session, SocketAddr, Session, SocketAddr) {
    let now = Instant::now();
    let a_addr: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let b_addr: SocketAddr = ANSWERER_ADDR.parse().unwrap();
    let offerer = Session::new(&SessionConfig::default(), now);
    let answerer = Session::new(&SessionConfig::default(), now);
    (offerer, a_addr, answerer, b_addr)
}

#[test]
fn two_in_process_endpoints_complete_ice_and_dtls() {
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();

    // The offerer adds video media and a host candidate, then produces an offer.
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    // The answerer accepts and produces an answer that the offerer applies.
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();

    // Both must reach ICE+DTLS connected purely over the in-memory shuttle.
    let connected = pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| {
        a.is_connected() && b.is_connected()
    });
    assert!(connected, "ICE+DTLS did not complete over the shuttle");
}

#[test]
fn srtp_rtp_media_flows_offerer_to_answerer() {
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();

    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();

    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // The offerer writes one encoded IDR access unit (Annex-B start code + IDR
    // NAL header `0x65` + slice bytes); str0m packetizes it into SRTP. The
    // answerer must surface it as decrypted media. A realistic access unit is
    // used because str0m's depacketizer only emits a complete frame, not a bare
    // parameter-set NAL.
    let mut payload = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    payload.extend(std::iter::repeat_n(0xABu8, 1200));
    a.write_video_sample(&payload, true, now).unwrap();

    let delivered = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() > 0
    });
    assert!(
        delivered,
        "SRTP-protected RTP media did not reach the answerer"
    );
    assert!(
        b.received_media_count() > 0,
        "answerer surfaced at least one decrypted media frame"
    );
    let frame = b
        .take_received_media()
        .expect("a decrypted frame is buffered");
    assert!(!frame.data.is_empty(), "the decrypted frame carries bytes");
}

#[test]
fn answer_is_str0m_native_ipv6_first_and_bundled() {
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    // str0m's own answer: BUNDLE + rtcp-mux + a real fingerprint + the answerer
    // as DTLS server (setup:passive). Real reachability rides the ICE candidate
    // line; the dummy `c=IN IP4 0.0.0.0` is the WebRTC norm (the addresses are in
    // `a=candidate`, never the `c=` line — RFC 8839 §4.3.2).
    assert!(answer.contains("a=group:BUNDLE"), "answer:\n{answer}");
    assert!(answer.contains("a=rtcp-mux"));
    assert!(answer.contains("a=fingerprint:"));
    assert!(
        answer.contains("a=setup:passive"),
        "answerer is the DTLS server"
    );
    // The gathered v6 host candidate is advertised (IPv6-first reachability,
    // ADR-0042); str0m carries the real address in the candidate, not `c=`.
    assert!(
        answer.contains("typ host") && answer.contains("::1"),
        "v6 host candidate present in:\n{answer}"
    );
}

#[test]
fn endpoint_config_rejects_turn_without_credentials() {
    use multiview_webrtc::config::{EndpointConfig, IceServer, IceServerKind};
    let bad = IceServer {
        kind: IceServerKind::Turn,
        addr: "[2001:db8::1]:3478".parse().unwrap(),
        credentials: None,
    };
    let cfg = EndpointConfig {
        ice_servers: vec![bad],
        ..EndpointConfig::default()
    };
    assert!(
        cfg.validate().is_err(),
        "TURN without credentials is rejected"
    );
}

#[test]
#[ignore = "live network: binds a real dual-stack UDP socket; hardware-gated"]
fn endpoint_binds_dual_stack_socket() {
    use multiview_webrtc::config::EndpointConfig;
    use multiview_webrtc::transport::WebRtcEndpoint;
    // Port 0 = ephemeral; the bind must be [::] dual-stack, never 0.0.0.0.
    let cfg = EndpointConfig {
        udp_port: 0,
        ..EndpointConfig::default()
    };
    let endpoint = WebRtcEndpoint::bind(cfg).expect("dual-stack bind");
    let local = endpoint.local_addr().expect("bound addr");
    assert!(local.is_ipv6(), "bound IPv6 dual-stack, not 0.0.0.0");
}
