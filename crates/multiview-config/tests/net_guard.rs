//! SSRF dial-guard unit tests (SEC-02/SEC-04, CWE-918/CWE-20) for the shared
//! `multiview_config::device::net_guard` address screen.
//!
//! The guard is the single source of truth both `Device::validate` (config
//! load) and the control-plane dial sites (device poller + Cast session actor)
//! consult before an outbound connection. These tests pin its behaviour: a
//! strict default-deny of every internal range on the RESOLVED IP (so a
//! DNS-rebind answer is caught, not just a literal), never-legitimate ranges
//! that no operator allowlist can re-enable, and an operator CIDR allowlist
//! that re-enables only the private/ULA LAN ranges.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::net::IpAddr;

use multiview_config::device::net_guard::{
    resolve_and_screen, screen_ip, screen_resolved, DialPolicy, HostResolver,
};

/// Parse an IP literal for a test table.
fn ip(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|_| panic!("`{text}` is an IP literal"))
}

/// The never-legitimate + internal ranges a default (empty-allowlist) policy
/// must reject on a resolved address.
const BLOCKED: &[&str] = &[
    // Never-legitimate (no allowlist can re-enable these).
    "127.0.0.1",
    "127.10.20.30",
    "::1",
    "169.254.169.254", // cloud-metadata IMDS
    "169.254.42.1",    // IPv4 link-local
    "fe80::1",         // IPv6 link-local
    "0.0.0.0",
    "::",
    "239.0.0.9",              // IPv4 multicast
    "ff02::1",                // IPv6 multicast
    "255.255.255.255",        // IPv4 broadcast
    "::ffff:127.0.0.1",       // IPv4-mapped loopback bypass
    "::ffff:169.254.169.254", // IPv4-mapped IMDS bypass
    // Private / ULA / CGNAT (allowlistable, but denied by default).
    "10.0.0.8",
    "172.16.0.8",
    "192.168.0.8",
    "100.64.0.8", // CGNAT (RFC 6598)
    "fd00::8",
    "fd00:db8::42",
];

/// Globally-routable addresses (and the documentation ranges the test suite
/// uses as public stand-ins) a default policy must accept.
const PUBLIC: &[&str] = &[
    "198.51.100.8", // TEST-NET-2 (public stand-in)
    "203.0.113.8",  // TEST-NET-3
    "1.1.1.1",
    "8.8.8.8",
    "2001:db8::8", // documentation (public stand-in)
    "2606:4700:4700::1111",
];

#[test]
fn default_policy_denies_every_internal_range() {
    let policy = DialPolicy::deny_internal();
    for text in BLOCKED {
        assert!(
            screen_ip(ip(text), &policy).is_err(),
            "default-deny must reject {text}"
        );
    }
}

#[test]
fn default_policy_accepts_global_unicast() {
    let policy = DialPolicy::deny_internal();
    for text in PUBLIC {
        screen_ip(ip(text), &policy)
            .unwrap_or_else(|err| panic!("default-deny must accept public {text}: {err}"));
    }
}

#[test]
fn screen_resolved_is_fail_closed_over_a_mixed_answer() {
    let policy = DialPolicy::deny_internal();
    // A single blocked IP among otherwise-public answers rejects the whole set
    // (a DNS answer cannot smuggle in a private target).
    assert!(
        screen_resolved([ip("198.51.100.8"), ip("10.0.0.8")], &policy).is_err(),
        "a mixed answer with one private IP must be rejected"
    );
    screen_resolved([ip("198.51.100.8"), ip("203.0.113.8")], &policy)
        .expect("an all-public answer is accepted");
}

#[test]
fn operator_allowlist_reenables_only_private_and_ula_ranges() {
    let policy = DialPolicy::from_cidrs(["192.168.0.0/16", "fd00:db8::/32"])
        .expect("valid operator dial allowlist");

    // Allowlisted LAN ranges are now reachable.
    screen_ip(ip("192.168.0.8"), &policy).expect("allowlisted RFC1918 host reachable");
    screen_ip(ip("fd00:db8::42"), &policy).expect("allowlisted ULA host reachable");

    // A private range NOT in the allowlist stays denied.
    assert!(
        screen_ip(ip("10.0.0.8"), &policy).is_err(),
        "a non-allowlisted private range stays denied"
    );
}

#[test]
fn never_legitimate_ranges_are_not_allowlistable() {
    // Even if an operator allowlists the link-local / loopback blocks, the
    // cloud-metadata IP and loopback stay hard-blocked.
    let policy = DialPolicy::from_cidrs(["169.254.0.0/16", "127.0.0.0/8", "::1/128"])
        .expect("valid (if misguided) allowlist");
    for text in ["169.254.169.254", "127.0.0.1", "::1"] {
        assert!(
            screen_ip(ip(text), &policy).is_err(),
            "{text} is never-legitimate and cannot be allowlisted"
        );
    }
}

/// A scripted resolver: maps a host name to the A/AAAA answer under test.
struct FakeResolver(HashMap<String, Vec<IpAddr>>);

impl HostResolver for FakeResolver {
    fn resolve(&self, host: &str) -> std::io::Result<Vec<IpAddr>> {
        self.0.get(host).cloned().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no answer for {host}"),
            )
        })
    }
}

#[test]
fn resolve_and_screen_rejects_a_dns_rebind_answer() {
    let resolver = FakeResolver(HashMap::from([
        // A public NAME that answers with a private / loopback address — the
        // classic DNS-rebind SSRF the resolved-IP check exists to defeat.
        ("rebind-v4.example.test".to_owned(), vec![ip("10.0.0.5")]),
        ("rebind-v6.example.test".to_owned(), vec![ip("::1")]),
        (
            "rebind-mixed.example.test".to_owned(),
            vec![ip("198.51.100.8"), ip("192.168.1.5")],
        ),
        // A genuinely public host.
        ("real.example.test".to_owned(), vec![ip("198.51.100.8")]),
    ]));
    let policy = DialPolicy::deny_internal();

    for host in [
        "rebind-v4.example.test",
        "rebind-v6.example.test",
        "rebind-mixed.example.test",
    ] {
        assert!(
            resolve_and_screen(host, &policy, &resolver).is_err(),
            "DNS-rebind answer for {host} must be rejected once resolved"
        );
    }

    let vetted = resolve_and_screen("real.example.test", &policy, &resolver)
        .expect("a public host resolves and screens clean");
    assert_eq!(
        vetted,
        vec![ip("198.51.100.8")],
        "returns the vetted answer"
    );
}
