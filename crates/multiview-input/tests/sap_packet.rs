//! RFC 2974 **SAP packet codec** tests — parse/encode round-trip, the golden
//! wire vectors, the `auth_len`-as-32-bit-WORDS regression (NOT VLC's byte
//! bug), and the security rejections (encrypted, compressed, zero-hash, short
//! buffer, wrong version).
//!
//! SAP = **Session Announcement Protocol** (RFC 2974), *not* subtitles. See
//! [ADR-0041] + `docs/research/sap-discovery.md` §3.
//!
//! [ADR-0041]: ../../docs/decisions/ADR-0041.md
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroU16;

use proptest::prelude::*;

use multiview_input::sap::packet::{SAP_VERSION, SDP_MIME_TYPE};
use multiview_input::sap::{SapError, SapMessageType, SapPacket};

/// A representative SDP body (opaque to the codec — begins `v=0`).
const SDP: &[u8] = b"v=0\r\no=- 1 1 IN IP4 192.0.2.1\r\ns=Multiview Test\r\nt=0 0\r\n";

/// Build a common IPv4 SAP header prefix (flags, auth-len, hash, 4-byte origin).
fn v4_header(flags: u8, auth_len: u8, hash: u16, origin: Ipv4Addr) -> Vec<u8> {
    let mut b = vec![flags, auth_len];
    b.extend_from_slice(&hash.to_be_bytes());
    b.extend_from_slice(&origin.octets());
    b
}

#[test]
fn parses_ipv4_announcement_with_omitted_payload_type() {
    let mut bytes = v4_header(0x20, 0x00, 0x1234, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).expect("valid announcement parses");
    assert_eq!(pkt.message_type, SapMessageType::Announcement);
    assert_eq!(pkt.msg_id_hash.get(), 0x1234);
    assert_eq!(pkt.origin, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
    assert_eq!(
        pkt.payload_type, None,
        "an SDP body beginning v=0 omits the MIME type"
    );
    assert_eq!(pkt.payload.as_slice(), SDP);
}

#[test]
fn encode_is_the_inverse_of_parse_for_the_golden_vector() {
    let mut bytes = v4_header(0x20, 0x00, 0x1234, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(
        pkt.encode(),
        bytes,
        "encode(parse(x)) == x for the golden vector"
    );
}

#[test]
fn parses_explicit_application_sdp_payload_type() {
    let mut bytes = v4_header(0x20, 0x00, 0x0001, Ipv4Addr::new(203, 0, 113, 5));
    bytes.extend_from_slice(SDP_MIME_TYPE.as_bytes());
    bytes.push(0x00); // NUL terminator
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(pkt.payload_type.as_deref(), Some("application/sdp"));
    assert_eq!(pkt.payload.as_slice(), SDP);
    assert_eq!(
        pkt.encode(),
        bytes,
        "explicit MIME round-trips byte-identically"
    );
}

#[test]
fn parses_ipv6_origin_when_address_bit_set() {
    let origin = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);
    let mut bytes = vec![0x30u8, 0x00]; // V=1 (top 3 bits), A=1 (IPv6 origin)
    bytes.extend_from_slice(&0xBEEFu16.to_be_bytes());
    bytes.extend_from_slice(&origin.octets());
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(pkt.origin, IpAddr::V6(origin));
    assert_eq!(pkt.msg_id_hash.get(), 0xBEEF);
    assert_eq!(pkt.encode(), bytes);
}

#[test]
fn parses_deletion_message_type() {
    let mut bytes = v4_header(0x24, 0x00, 0x00AA, Ipv4Addr::new(192, 0, 2, 9)); // T=1 (0x04)
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).unwrap();
    assert_eq!(pkt.message_type, SapMessageType::Deletion);
}

#[test]
fn auth_len_is_skipped_as_32bit_words_not_bytes() {
    // auth_len = 1 word = 4 bytes of auth data. A compliant parser skips 4 bytes;
    // VLC's `buf += auth_len` bug skips 1. Placing exactly 4 auth bytes then the
    // SDP proves the skip is words*4 (the SDP still lands and begins `v=0`).
    let mut bytes = v4_header(0x20, 0x01, 0x1000, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // 1 word (4 bytes) of auth
    bytes.extend_from_slice(SDP);
    let pkt = SapPacket::parse(&bytes).expect("auth is skipped, the SDP parses");
    assert_eq!(
        pkt.payload_type, None,
        "4 auth bytes skipped => the body begins v=0 (type omitted)"
    );
    assert_eq!(
        pkt.payload.as_slice(),
        SDP,
        "auth_len counts 32-bit WORDS (skip words*4), never VLC's byte count"
    );
}

#[test]
fn rejects_encrypted_packets() {
    let mut bytes = v4_header(0x22, 0x00, 0x0001, Ipv4Addr::new(192, 0, 2, 1)); // E=1 (0x02)
    bytes.extend_from_slice(SDP);
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::Encrypted));
}

#[test]
fn rejects_compressed_packets_as_unsupported() {
    let mut bytes = v4_header(0x21, 0x00, 0x0001, Ipv4Addr::new(192, 0, 2, 1)); // C=1 (0x01)
    bytes.extend_from_slice(SDP);
    assert!(matches!(
        SapPacket::parse(&bytes),
        Err(SapError::CompressionUnsupported)
    ));
}

#[test]
fn rejects_zero_message_id_hash() {
    let mut bytes = v4_header(0x20, 0x00, 0x0000, Ipv4Addr::new(192, 0, 2, 1));
    bytes.extend_from_slice(SDP);
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::ZeroHash));
}

#[test]
fn rejects_wrong_version() {
    let mut bytes = v4_header(0x40, 0x00, 0x0001, Ipv4Addr::new(192, 0, 2, 1)); // V=2 (2<<5)
    bytes.extend_from_slice(SDP);
    assert_eq!(SapPacket::parse(&bytes), Err(SapError::BadVersion(2)));
}

#[test]
fn rejects_short_buffers_without_panicking() {
    let cases: [&[u8]; 6] = [
        &[],
        &[0x20],
        &[0x20, 0x00],
        &[0x20, 0x00, 0x12],
        &[0x20, 0x00, 0x12, 0x34],
        &[0x20, 0x00, 0x12, 0x34, 0xC0, 0x00, 0x02],
    ];
    for truncated in cases {
        assert!(
            SapPacket::parse(truncated).is_err(),
            "short buffer {truncated:?} must error, never panic"
        );
    }
}

#[test]
fn encode_never_emits_zero_hash_encryption_or_compression() {
    let pkt = SapPacket {
        message_type: SapMessageType::Announcement,
        msg_id_hash: NonZeroU16::new(0x4d56).unwrap(),
        origin: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)),
        payload_type: Some(SDP_MIME_TYPE.to_owned()),
        payload: SDP.to_vec(),
    };
    let bytes = pkt.encode();
    let flags = bytes[0];
    assert_eq!(
        flags >> 5,
        SAP_VERSION,
        "version field is the top 3 bits = 1"
    );
    assert_eq!(flags & 0x02, 0, "E (encryption) is never set on emit");
    assert_eq!(flags & 0x01, 0, "C (compression) is never set on emit");
    assert_eq!(bytes[1], 0, "our announcer emits auth_len = 0");
    assert_ne!(
        u16::from_be_bytes([bytes[2], bytes[3]]),
        0,
        "the message-id hash is never emitted as 0"
    );
}

/// Arbitrary origin: an IPv4 (A=0) or IPv6 (A=1) address.
fn arb_origin() -> impl Strategy<Value = IpAddr> {
    prop_oneof![
        any::<[u8; 4]>().prop_map(|o| IpAddr::V4(Ipv4Addr::from(o))),
        any::<[u8; 16]>().prop_map(|o| IpAddr::V6(Ipv6Addr::from(o))),
    ]
}

/// Arbitrary well-formed packet. The payload-type variants mirror the two real
/// forms: an omitted type (body begins `v=0`) or an explicit MIME type (no NUL,
/// not starting `v=0`) followed by an opaque payload.
fn arb_packet() -> impl Strategy<Value = SapPacket> {
    let msg_type = prop_oneof![
        Just(SapMessageType::Announcement),
        Just(SapMessageType::Deletion),
    ];
    let hash = (1u16..=u16::MAX).prop_map(|h| NonZeroU16::new(h).unwrap());
    let payload_variant = prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..256).prop_map(|tail| {
            let mut p = b"v=0".to_vec();
            p.extend_from_slice(&tail);
            (None, p)
        }),
        (
            prop_oneof![
                Just("application/sdp"),
                Just("text/plain"),
                Just("application/x-multiview"),
            ],
            proptest::collection::vec(any::<u8>(), 0..256),
        )
            .prop_map(|(m, p)| (Some(m.to_owned()), p)),
    ];
    (msg_type, hash, arb_origin(), payload_variant).prop_map(
        |(message_type, msg_id_hash, origin, (payload_type, payload))| SapPacket {
            message_type,
            msg_id_hash,
            origin,
            payload_type,
            payload,
        },
    )
}

proptest! {
    /// Round-trip identity: `parse(encode(pkt)) == pkt` over all valid inputs.
    #[test]
    fn round_trip_encode_then_parse_is_identity(pkt in arb_packet()) {
        let encoded = pkt.encode();
        let parsed = SapPacket::parse(&encoded).expect("a well-formed packet parses");
        prop_assert_eq!(parsed, pkt);
    }
}
