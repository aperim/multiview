//! Failing-first tests for the shared sans-IO **TURN relay driver**
//! ([`multiview_webrtc::turn::TurnRelayDriver`]) — the glue both the WHIP ingest
//! endpoint and the WHEP egress preview driver use to run their configured TURN
//! clients over their own UDP socket and harvest the allocated relay candidates
//! (ADR-0048 §5.1). The `TurnClient` is the shared machinery; this driver wraps a
//! client-per-TURN-server with the feed/pump/harvest steps so neither endpoint
//! re-implements the loop (and the TURN client is never duplicated).
//!
//! Offline: no socket. The driver emits datagrams via [`poll_transmit`], consumes
//! datagrams via [`feed`], and the harvested relays are read back — all driven
//! against the in-process [`FakeTurnServer`].
#![cfg(feature = "native")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions
)]

mod fake_turn;

use std::net::SocketAddr;
use std::time::Instant;

use fake_turn::FakeTurnServer;
use multiview_webrtc::config::{EndpointConfig, IceServer, TurnCredentials};
use multiview_webrtc::turn::TurnRelayDriver;

const SERVER: &str = "[2001:db8::1]:3478";
const RELAY: &str = "[2001:db8::1]:49152";

#[test]
fn build_one_driver_client_per_turn_server_stun_skipped() {
    // A STUN server needs no TURN client (str0m gathers srflx from
    // bound/advertised addresses); a TURN server yields one driven client.
    let config = EndpointConfig {
        ice_servers: vec![
            IceServer::stun("[2001:db8::53]:3478".parse().unwrap()),
            IceServer::turn(
                "[2001:db8::55]:3478".parse().unwrap(),
                TurnCredentials::long_term("u", "p"),
            ),
        ],
        ..EndpointConfig::default()
    };
    let driver = TurnRelayDriver::from_config(&config, Instant::now());
    assert_eq!(driver.client_count(), 1, "one client for the one TURN server");
    assert!(!driver.is_empty());
}

#[test]
fn no_turn_servers_yields_an_empty_driver() {
    let config = EndpointConfig {
        ice_servers: vec![IceServer::stun("[2001:db8::53]:3478".parse().unwrap())],
        ..EndpointConfig::default()
    };
    let driver = TurnRelayDriver::from_config(&config, Instant::now());
    assert!(driver.is_empty());
    assert_eq!(driver.client_count(), 0);
}

#[test]
fn driver_runs_the_allocation_and_harvests_the_relay() {
    // The full proof the operator's TURN requirement rides *inside* an endpoint
    // driver: build the driver from config, pump its sans-IO output to the fake
    // TURN server, feed the replies back, and confirm the driver harvests the
    // allocated relay address (which the endpoint then offers as a relay
    // candidate). Mirrors the WHIP `feed_turn`/`pump_turn` loop.
    let now = Instant::now();
    let server_addr: SocketAddr = SERVER.parse().unwrap();
    let relay_addr: SocketAddr = RELAY.parse().unwrap();
    let mut server = FakeTurnServer::new(server_addr, relay_addr, "alice", "s3cret", "example.org");

    let config = EndpointConfig {
        ice_servers: vec![IceServer::turn(
            server_addr,
            TurnCredentials::long_term("alice", "s3cret"),
        )],
        ..EndpointConfig::default()
    };
    let mut driver = TurnRelayDriver::from_config(&config, now);

    // Pump → fake-server → feed, bounded so a regression cannot spin forever.
    let mut learned: Vec<SocketAddr> = Vec::new();
    for _ in 0..32 {
        while let Some((dst, payload)) = driver.poll_transmit(now) {
            assert_eq!(dst, server_addr, "the driver targets the configured server");
            if let Some(reply) = server.handle(&payload) {
                // A datagram from the TURN server feeds its client; the driver
                // returns whether it consumed it (true here).
                let consumed = driver.feed(server_addr, &reply, now);
                assert!(consumed, "the TURN-server reply is consumed by the driver");
            }
        }
        learned.extend(driver.take_new_relays());
        if !learned.is_empty() {
            break;
        }
    }
    assert!(
        server.saw_authenticated_allocate(),
        "the driver retried the Allocate with MESSAGE-INTEGRITY"
    );
    assert_eq!(
        learned,
        vec![relay_addr],
        "the driver harvested the allocated relay exactly once"
    );
    // A media datagram (not from the TURN server) is NOT consumed by the driver.
    let media_src: SocketAddr = "[2001:db8::abc]:40000".parse().unwrap();
    assert!(
        !driver.feed(media_src, &[0u8, 1, 2, 3], now),
        "a non-TURN datagram is left for the media path"
    );
}
