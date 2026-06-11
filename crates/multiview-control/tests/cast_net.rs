//! The CASTV2 wire codec behind the off-by-default `cast` feature (DEV-D2,
//! ADR-M011): 4-byte big-endian length prefix + protobuf `CastMessage`
//! (schema from the BSD-3-Clause Chromium Open Screen sources), exercised
//! over an in-memory duplex — no socket, no device.
#![cfg(feature = "cast")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_control::devices::cast::net::{read_frame, write_frame, MAX_FRAME_LEN};
use multiview_control::devices::cast::protocol::{CastFrame, NS_HEARTBEAT};

#[tokio::test]
async fn frames_round_trip_over_the_wire_codec() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (mut read_half, _w) = tokio::io::split(server);
    let (_r, mut write_half) = tokio::io::split(client);

    let frame = CastFrame {
        namespace: NS_HEARTBEAT.to_owned(),
        source: "sender-0".to_owned(),
        destination: "receiver-0".to_owned(),
        payload: "{\"type\":\"PING\"}".to_owned(),
    };
    write_frame(&mut write_half, &frame)
        .await
        .expect("the frame writes");
    let decoded = read_frame(&mut read_half).await.expect("the frame reads");
    assert_eq!(decoded, frame);
}

#[tokio::test]
async fn oversized_frames_are_rejected_not_buffered() {
    // Bounded memory: a length prefix beyond MAX_FRAME_LEN is an error — the
    // reader never allocates an attacker-controlled buffer.
    use tokio::io::AsyncWriteExt;

    let (client, server) = tokio::io::duplex(1024);
    let (mut read_half, _w) = tokio::io::split(server);
    let (_r, mut write_half) = tokio::io::split(client);

    let huge = u32::try_from(MAX_FRAME_LEN)
        .expect("fits u32")
        .saturating_add(1);
    write_half
        .write_all(&huge.to_be_bytes())
        .await
        .expect("the bogus prefix writes");
    let err = read_frame(&mut read_half)
        .await
        .expect_err("an oversized frame is rejected");
    assert!(
        err.to_string().contains("frame"),
        "the error names the frame bound: {err}"
    );
}
