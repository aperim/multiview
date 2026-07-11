//! SAP **socket transport** loopback validation (ADR-0041 §3/§5, `st2110`
//! feature).
//!
//! The listener binds an IPv6-first UDP socket, receives datagrams, parses each,
//! and folds announcements into the wait-free session table off the output-clock
//! data plane (inv #1/#10); the announcer builds + sends RFC 2974 packets. These
//! tests exercise the real sockets over **unicast loopback** (the multicast group
//! join is the live-network path, not reachable in this devcontainer). A
//! malformed datagram must be skipped without killing the receive loop, and a
//! spoofed inbound deletion must never withdraw a tracked session.
#![cfg(feature = "st2110")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use multiview_input::sap::announce::{announcement, deletion, stable_hash, AnnounceSchedule};
use multiview_input::sap::transport::{AnnouncedSession, SapAnnouncer, SapListener};

fn loopback6(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port)
}

/// Poll up to ~2 s for an async effect to land, so the tests never hang.
async fn wait_for(pred: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    pred()
}

#[tokio::test]
async fn loopback_announce_is_received_and_folded_into_the_table() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0\r\no=- 1 1 IN IP6 ff3e::1\r\ns=multiview\r\n".to_vec();
    let hash = stable_hash(&sdp);
    let pkt = announcement(hash, IpAddr::V6(Ipv6Addr::LOCALHOST), sdp.clone());
    announcer.send_to(&pkt, addr).await.unwrap();

    assert!(
        wait_for(|| table.len() == 1).await,
        "the announcement is received, parsed and discovered"
    );
    let inv = table.inventory();
    assert_eq!(inv[0].sdp, sdp);
    assert_eq!(inv[0].key.msg_id_hash, hash);
}

#[tokio::test]
async fn malformed_datagram_is_skipped_and_the_loop_survives() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    // Raw socket to inject garbage the SapAnnouncer would never build.
    let raw = tokio::net::UdpSocket::bind(loopback6(0)).await.unwrap();
    raw.send_to(&[0xFF, 0x00, 0x00, 0x01], addr).await.unwrap(); // bad version, too short
    raw.send_to(&[], addr).await.unwrap(); // empty datagram

    // A valid announcement after the garbage must still be discovered → the loop
    // survived the malformed inputs.
    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0 valid-after-garbage".to_vec();
    let pkt = announcement(
        stable_hash(&sdp),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
        sdp.clone(),
    );
    announcer.send_to(&pkt, addr).await.unwrap();

    assert!(
        wait_for(|| table.len() == 1).await,
        "the valid announcement lands after garbage — the receive loop never died"
    );
}

#[tokio::test]
async fn inbound_deletion_never_withdraws_a_tracked_session_over_the_wire() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0 tracked".to_vec();
    let hash = stable_hash(&sdp);
    let origin = IpAddr::V6(Ipv6Addr::LOCALHOST);
    announcer
        .send_to(&announcement(hash, origin, sdp.clone()), addr)
        .await
        .unwrap();
    assert!(wait_for(|| table.len() == 1).await, "session is tracked");

    // A spoofed deletion must be ignored (ADR-0041 §8 hijack guard).
    announcer
        .send_to(&deletion(hash, origin, sdp.clone()), addr)
        .await
        .unwrap();
    // Give the loop time to (not) act on it, then assert it survived.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        table.len(),
        1,
        "the tracked session survives a spoofed inbound deletion"
    );
}

#[tokio::test]
async fn listener_binds_ipv6_unspecified_dual_stack() {
    // ADR-0042 IPv6-first: bind [::] rather than 0.0.0.0.
    let listener = SapListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0))
        .await
        .unwrap();
    assert!(
        listener.local_addr().unwrap().is_ipv6(),
        "the listener binds an IPv6 socket"
    );
}

#[tokio::test]
async fn a_datagram_flood_is_rate_limited_before_the_expensive_fold() {
    use multiview_input::sap::SapSessionTable;
    use std::sync::Arc;

    // A high-capacity table so `len()` reflects the number of *folds* (not the
    // default 256 cap), and a TIGHT rate limit with a window far longer than the
    // test, so exactly `burst` datagrams are folded regardless of send timing —
    // fully deterministic (panel F4).
    let table = Arc::new(SapSessionTable::with_limits(100_000, 100_000));
    let listener = SapListener::bind(loopback6(0))
        .await
        .unwrap()
        .with_table(Arc::clone(&table))
        .with_rate_limit(8, Duration::from_secs(30));
    let addr = listener.local_addr().unwrap();
    tokio::spawn(listener.run());

    // Flood 200 DISTINCT valid announcements (distinct hashes) as fast as we can.
    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let origin = IpAddr::V6(Ipv6Addr::LOCALHOST);
    for i in 0..200u32 {
        let sdp = format!("v=0\r\no=- {i} {i} IN IP6 ff3e::1\r\ns=flood-{i}\r\n").into_bytes();
        let pkt = announcement(stable_hash(&sdp), origin, sdp);
        announcer.send_to(&pkt, addr).await.unwrap();
    }

    // Give the listener time to drain the socket and fold whatever the limiter
    // admits. Only the first `burst` (8) can enter the expensive fold path; the
    // rest are dropped cheaply BEFORE the O(n) RCU clone (inv #10).
    let bounded = wait_for(|| table.len() >= 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let folded = table.len();
    assert!(bounded && folded >= 1, "at least the burst is folded");
    assert!(
        folded <= 8,
        "a same-window flood folds at most the rate-limit burst (8), got {folded} of 200 — \
         the expensive fold is gated BEFORE it runs (F4 / inv #10)"
    );
}

#[tokio::test]
async fn announcer_run_loop_emits_the_first_cycle_immediately() {
    let listener = SapListener::bind(loopback6(0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let table = listener.table();
    tokio::spawn(listener.run());

    let announcer = SapAnnouncer::bind(loopback6(0)).await.unwrap();
    let sdp = b"v=0 scheduled".to_vec();
    let session = AnnouncedSession {
        hash: stable_hash(&sdp),
        origin: IpAddr::V6(Ipv6Addr::LOCALHOST),
        sdp: sdp.clone(),
        dest: addr,
    };
    // A 30 s cadence means the first announcement is sent immediately, before the
    // first (jittered) sleep — so the table populates without waiting a cycle.
    tokio::spawn(announcer.run(
        vec![session],
        AnnounceSchedule::new(Duration::from_secs(30)),
    ));

    assert!(
        wait_for(|| table.len() == 1).await,
        "the announce run loop emits its first cycle immediately"
    );
}
