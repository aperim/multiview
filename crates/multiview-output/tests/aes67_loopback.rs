//! AES67 / ST 2110-30 **loopback round-trip** (ADR-0033, feature `aes67`).
//!
//! Drives the real `Aes67UdpSender::serve` send loop over a `[::1]` UDP socket
//! and decodes the received datagrams with `multiview-input`'s pure RTP + L24
//! parsers (a dev-dependency) — encode here, transmit over a real socket, decode
//! there — proving the on-wire bytes are AES67-conformant end to end. Gated
//! behind `aes67` so the default CI build stays green without a socket leg.
//!
//! An ffmpeg AES67 counterpart on loopback is documented + `#[ignore]`d below;
//! it needs an ffmpeg binary and is run on demand (it is not part of the
//! socket-free default CI).

#![cfg(feature = "aes67")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::as_conversions,
    clippy::cast_precision_loss,
    clippy::float_cmp
)]

use std::net::{Ipv6Addr, SocketAddr};

use multiview_audio::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_output::aes67::transport::Aes67UdpSender;
use multiview_output::aes67::{Aes67Sender, PcmDepth};

use multiview_input::st2110::{pcm_to_f32, Aes3Format, RtpPacket, SampleDepth, V30Payload};

const FRAMES_PER_PACKET: usize = 48;

/// The engine's baked program block for this test: `FRAMES_PER_PACKET` stereo
/// frames of distinct unit-range values (so a mis-ordered or corrupt payload
/// shows up immediately).
fn program_samples() -> Vec<f32> {
    (0..(FRAMES_PER_PACKET * 2))
        .map(|i| (i as f32) / 300.0 - 0.16)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn aes67_send_loopback_roundtrips_over_a_real_udp_socket() {
    const PACKETS: usize = 8;
    let loopback = Ipv6Addr::LOCALHOST;

    // The receiver: a plain UDP socket on an ephemeral [::1] port.
    let rx = tokio::net::UdpSocket::bind(SocketAddr::new(loopback.into(), 0))
        .await
        .expect("bind rx");
    let rx_addr = rx.local_addr().expect("rx addr");

    // The sender: the real Aes67UdpSender targeting the receiver.
    let tx = Aes67UdpSender::bind(SocketAddr::new(loopback.into(), 0), rx_addr)
        .await
        .expect("bind tx");

    // A program-audio sender pre-loaded with several packets of real audio.
    let samples = program_samples();
    let block = AudioBlock::from_interleaved(
        AudioFormat::new(48_000, ChannelLayout::Stereo),
        samples.clone(),
    )
    .unwrap();
    let mut sender =
        Aes67Sender::new(2, PcmDepth::L24, 97, 0x0BAD_F00D, 48_000, FRAMES_PER_PACKET, 4_800)
            .expect("valid aes67 config");
    let handle = sender.handle();
    for _ in 0..PACKETS {
        handle.push(&block);
    }

    // Drive the real send loop at the sender's derived media-clock cadence
    // (1 ms for 48@48k); `pending()` keeps it running until we abort it after
    // receiving every packet (deterministic — no timing-based counting).
    let serve = tokio::spawn(async move {
        let _ = tx
            .serve(&mut sender, std::future::pending::<()>())
            .await;
    });

    let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
    let mut buf = vec![0u8; 2048];
    let mut prev_seq: Option<u16> = None;
    for _ in 0..PACKETS {
        let n = rx.recv(&mut buf).await.expect("recv datagram");
        let datagram = &buf[..n];

        let rtp = RtpPacket::parse(datagram).expect("a conformant RTP packet");
        assert!(
            !rtp.header.marker,
            "continuous stream: marker=0 on the wire"
        );
        assert_eq!(rtp.header.payload_type, 97);
        if let Some(seq) = prev_seq {
            assert_eq!(
                rtp.header.sequence,
                seq.wrapping_add(1),
                "sequence advances by 1 across the socket"
            );
        }
        prev_seq = Some(rtp.header.sequence);

        let payload = V30Payload::parse(rtp.payload, format).expect("whole L24 groups");
        let decoded = pcm_to_f32(&payload);
        assert_eq!(decoded.len(), samples.len());
        for (want, got) in samples.iter().zip(&decoded) {
            assert!(
                (want - got).abs() < 1.0e-4,
                "L24 survives the encode->socket->decode round-trip: want {want}, got {got}",
            );
        }
    }

    serve.abort();
}
