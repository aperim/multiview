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

mod fake_turn;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use fake_turn::FakeTurnServer as FakeTurn;
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
    a.write_video_sample(&payload, true, 0, now).unwrap();

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
fn video_samples_carry_advancing_rtp_timestamps() {
    // ADR-P006: preview egress re-stamps each access unit at the negotiated
    // payload type on the 90 kHz clock with the sample's own RTP timestamp — not
    // a constant 0. Two distinct-timestamp samples must both reach the answerer.
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

    // Two complete IDR access units at two distinct 90 kHz timestamps.
    let mut au0 = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au0.extend(std::iter::repeat_n(0xABu8, 1200));
    let mut au1 = vec![0x00u8, 0x00, 0x00, 0x01, 0x65];
    au1.extend(std::iter::repeat_n(0xCDu8, 1200));
    // rtp_ts in 90 kHz units: 0 and 3000 (≈ 33 ms apart).
    a.write_video_sample(&au0, true, 0, now).unwrap();
    let delivered_first = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() >= 1
    });
    assert!(delivered_first, "first sample must arrive");
    a.write_video_sample(&au1, false, 3000, now).unwrap();
    let delivered_second = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() >= 2
    });
    assert!(
        delivered_second,
        "a second sample at a distinct timestamp must also arrive (the RTP \
         timestamp advanced, not pinned to 0)"
    );
}

#[test]
fn audio_opus_samples_flow_to_the_answerer() {
    // ADR-P006: preview carries Opus audio alongside video. An audio sample
    // written to the Opus media must reach the answerer as decrypted media.
    let (mut a, a_addr, mut b, b_addr) = offerer_answerer();
    let now = Instant::now();
    a.add_host_candidate(a_addr).unwrap();
    b.add_host_candidate(b_addr).unwrap();
    let offer = a
        .create_offer(&[MediaKind::Video, MediaKind::Audio])
        .unwrap();
    let answer = b.accept_offer(&offer).unwrap();
    a.accept_answer(&answer).unwrap();
    assert!(
        pump_until(&mut a, a_addr, &mut b, b_addr, now, |a, b| a.is_connected()
            && b.is_connected()),
        "must connect first"
    );

    // A 20 ms Opus frame is 960 samples at the 48 kHz RTP clock.
    let opus_frame = vec![0xF8u8; 80];
    a.write_audio_sample(&opus_frame, 0, now).unwrap();
    a.write_audio_sample(&opus_frame, 960, now).unwrap();
    let delivered = pump_until(&mut a, a_addr, &mut b, b_addr, now, |_a, b| {
        b.received_media_count() >= 1
    });
    assert!(delivered, "Opus audio media did not reach the answerer");
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
fn relay_candidate_from_turn_appears_in_the_offer() {
    // The operator's NAT-traversal path: the in-crate TURN client allocates a
    // relay (proven separately in `turn_client.rs` against a fake server); the
    // learned relay address is registered with str0m via `add_relay_candidate`
    // and must surface in the offer SDP as a `typ relay` candidate (IPv6-first).
    let now = Instant::now();
    let mut a = Session::new(&SessionConfig::default(), now);
    let host: SocketAddr = OFFERER_ADDR.parse().unwrap();
    // A relayed transport address as a TURN Allocate would yield (v6 relay), and
    // the local socket the relayed traffic egresses from.
    let relayed: SocketAddr = "[2001:db8::a11]:49152".parse().unwrap();
    a.add_host_candidate(host).unwrap();
    a.add_relay_candidate(relayed, host).unwrap();
    let offer = a.create_offer(&[MediaKind::Video]).unwrap();
    assert!(
        offer.contains("typ relay"),
        "the TURN relay candidate is advertised in:\n{offer}"
    );
    assert!(
        offer.contains("2001:db8::a11"),
        "the v6 relayed address is present in:\n{offer}"
    );
}

#[test]
fn turn_allocate_drives_to_a_relay_that_can_be_registered() {
    // End-to-end (offline) TURN: drive the in-crate client to an allocation vs an
    // in-process fake TURN server, then register the learned relay with a session
    // — the same wiring the live endpoint uses, with the socket replaced by a
    // direct shuttle.
    use multiview_webrtc::turn::message::{Class, Method, StunMessage};
    use multiview_webrtc::turn::{TurnClient, TurnCredential, TurnOutput, TurnState};

    let now = Instant::now();
    let server: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
    let relay: SocketAddr = "[2001:db8::1]:49152".parse().unwrap();
    let mut client = TurnClient::new(server, TurnCredential::static_credential("u", "p", None));
    let mut fake = FakeTurn::new(server, relay, "u", "p", "realm");

    let mut learned = None;
    for _ in 0..32 {
        if let TurnOutput::Transmit { payload, .. } = client.poll_output(now) {
            if let Some(reply) = fake.handle(&payload) {
                client.handle_input(&reply, now).unwrap();
            }
        }
        if let TurnState::Allocated { relay, .. } = client.state() {
            learned = Some(relay);
            break;
        }
    }
    let relay_addr = learned.expect("TURN allocation reached a relay");
    assert_eq!(relay_addr, relay);

    // Register the relay with a real session — proves the relay address is a
    // valid str0m relay candidate.
    let local: SocketAddr = OFFERER_ADDR.parse().unwrap();
    let mut session = Session::new(&SessionConfig::default(), now);
    session.add_relay_candidate(relay_addr, local).unwrap();
    let offer = session.create_offer(&[MediaKind::Video]).unwrap();
    assert!(offer.contains("typ relay"), "relay candidate registered");

    // Keep the fake-server response codec honest.
    let probe = StunMessage::request(Method::Binding).to_bytes(None);
    assert_eq!(StunMessage::parse(&probe).unwrap().class(), Class::Request);
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
