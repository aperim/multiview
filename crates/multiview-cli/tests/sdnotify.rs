//! `sd_notify` protocol tests (DEV-B5 / ADR-0045): the dependency-free systemd
//! readiness/watchdog datagram protocol — message encoding (exact bytes),
//! `WATCHDOG_USEC`/`WATCHDOG_PID` parsing, the tick-gated watchdog liveness
//! gate, and real `SOCK_DGRAM` delivery to a path (and, on Linux, abstract)
//! `AF_UNIX` socket. No systemd needed: the protocol IS the datagram.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::os::unix::net::UnixDatagram;
use std::time::Duration;

use multiview_cli::sdnotify::{
    encode_states, watchdog_interval_from, Notifier, NotifyState, WatchdogGate,
};

// ---------------------------------------------------------------------------
// Message encoding — the exact wire bytes
// ---------------------------------------------------------------------------

#[test]
fn encode_ready_is_the_exact_protocol_line() {
    assert_eq!(encode_states(&[NotifyState::Ready]), b"READY=1".to_vec());
}

#[test]
fn encode_joins_states_with_newlines() {
    let bytes = encode_states(&[
        NotifyState::Ready,
        NotifyState::Status("node up: 1 head lit"),
    ]);
    assert_eq!(bytes, b"READY=1\nSTATUS=node up: 1 head lit".to_vec());
}

#[test]
fn encode_watchdog_and_stopping() {
    assert_eq!(
        encode_states(&[NotifyState::Watchdog]),
        b"WATCHDOG=1".to_vec()
    );
    assert_eq!(
        encode_states(&[NotifyState::Stopping]),
        b"STOPPING=1".to_vec()
    );
}

#[test]
fn encode_strips_newlines_from_status_text() {
    // A newline inside STATUS would inject a forged protocol line; the
    // encoder must neutralize it.
    let bytes = encode_states(&[NotifyState::Status("a\nREADY=1")]);
    assert_eq!(bytes, b"STATUS=a READY=1".to_vec());
}

// ---------------------------------------------------------------------------
// Watchdog env parsing — WATCHDOG_USEC / WATCHDOG_PID
// ---------------------------------------------------------------------------

#[test]
fn watchdog_interval_halves_the_usec_budget() {
    // systemd's own guidance: ping at half WatchdogSec.
    assert_eq!(
        watchdog_interval_from(Some("3000000"), None, 42),
        Some(Duration::from_millis(1_500))
    );
}

#[test]
fn watchdog_interval_respects_the_pid_filter() {
    assert_eq!(
        watchdog_interval_from(Some("3000000"), Some("42"), 42),
        Some(Duration::from_millis(1_500)),
        "a matching WATCHDOG_PID keeps the watchdog"
    );
    assert_eq!(
        watchdog_interval_from(Some("3000000"), Some("41"), 42),
        None,
        "a mismatched WATCHDOG_PID means the watchdog is for another process"
    );
}

#[test]
fn watchdog_interval_rejects_zero_and_garbage() {
    assert_eq!(watchdog_interval_from(Some("0"), None, 1), None);
    assert_eq!(watchdog_interval_from(Some("banana"), None, 1), None);
    assert_eq!(watchdog_interval_from(None, None, 1), None);
}

// ---------------------------------------------------------------------------
// The tick-gated liveness gate: pings only while the output clock advances
// ---------------------------------------------------------------------------

#[test]
fn watchdog_gate_pings_only_while_ticks_advance() {
    let mut gate = WatchdogGate::new();
    assert!(
        !gate.should_ping(0),
        "no tick yet: no ping (startup is\
        covered by the unit's start timeout, not the watchdog)"
    );
    assert!(gate.should_ping(1), "the clock advanced: ping");
    assert!(gate.should_ping(60), "still advancing: ping");
    assert!(
        !gate.should_ping(60),
        "the clock stalled: WITHHOLD the ping\
        so systemd restarts the node (invariant #1's enforcement)"
    );
    assert!(gate.should_ping(61), "advancing again: ping resumes");
}

// ---------------------------------------------------------------------------
// Real datagram delivery
// ---------------------------------------------------------------------------

#[test]
fn notifier_sends_ready_to_a_path_socket() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("notify.sock");
    let receiver = UnixDatagram::bind(&path).expect("bind receiver");
    receiver
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");

    let notifier = Notifier::from_address(Some(path.as_os_str()));
    assert!(notifier.is_active());
    notifier.notify(&[NotifyState::Ready]);

    let mut buf = [0_u8; 64];
    let n = receiver.recv(&mut buf).expect("datagram arrives");
    assert_eq!(&buf[..n], b"READY=1");
}

#[cfg(target_os = "linux")]
#[test]
fn notifier_sends_to_an_abstract_socket() {
    use std::os::linux::net::SocketAddrExt as _;
    // An abstract-namespace name unique to this test run; NOTIFY_SOCKET
    // encodes it with a leading '@'.
    let name = format!("multiview-sdnotify-test-{}", std::process::id());
    let addr =
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).expect("abstract addr");
    let receiver = UnixDatagram::bind_addr(&addr).expect("bind abstract receiver");
    receiver
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");

    let notify_socket = format!("@{name}");
    let notifier = Notifier::from_address(Some(std::ffi::OsStr::new(&notify_socket)));
    assert!(notifier.is_active());
    notifier.notify(&[NotifyState::Stopping]);

    let mut buf = [0_u8; 64];
    let n = receiver.recv(&mut buf).expect("datagram arrives");
    assert_eq!(&buf[..n], b"STOPPING=1");
}

#[test]
fn notifier_without_an_address_is_inert() {
    let notifier = Notifier::from_address(None);
    assert!(!notifier.is_active());
    // Must be a silent no-op (the non-systemd / container case).
    notifier.notify(&[NotifyState::Ready, NotifyState::Watchdog]);
}
