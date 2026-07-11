//! `RtpReceiver` multicast-join dispatch (ADR-0042 IPv6-first; `st2110` feature).
//!
//! The AES67 / ST 2110 RX wiring binds a receive socket and joins the media
//! multicast group named on the SDP `c=` line — which may be IPv4 or IPv6. So the
//! pipeline wants ONE family-agnostic call, `rx.join_multicast(group)`, that
//! dispatches on the address family. These tests bind a socket per family and
//! join a real group (this devcontainer's `eth0` is multicast-capable); they run
//! under the `st2110` feature only.
#![cfg(feature = "st2110")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use multiview_input::st2110::transport::RtpReceiver;

fn any_v4() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
}

fn any_v6() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
}

#[tokio::test]
async fn join_multicast_dispatches_ipv4_by_family() {
    let rx = RtpReceiver::bind(any_v4()).await.unwrap();
    // The AES67/Dante local-scope group.
    rx.join_multicast(IpAddr::V4(Ipv4Addr::new(239, 255, 255, 255)))
        .expect("v4 group join dispatches to join_multicast_v4");
}

#[tokio::test]
async fn join_multicast_dispatches_ipv6_by_family() {
    let rx = RtpReceiver::bind(any_v6()).await.unwrap();
    // A global-scope IPv6 media group (ADR-0042 c=IN IP6 [ff3e::1]).
    rx.join_multicast(IpAddr::V6(Ipv6Addr::new(0xff3e, 0, 0, 0, 0, 0, 0, 1)))
        .expect("v6 group join dispatches to join_multicast_v6");
}

#[tokio::test]
async fn join_multicast_v6_joins_an_ipv6_group_on_the_default_interface() {
    let rx = RtpReceiver::bind(any_v6()).await.unwrap();
    // The IPv6 SAP group, site-local scope.
    rx.join_multicast_v6(Ipv6Addr::new(0xff05, 0, 0, 0, 0, 0, 2, 0x7ffe), 0)
        .expect("explicit v6 join on interface 0 (default)");
}
