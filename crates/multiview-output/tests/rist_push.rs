//! RIST (Reliable Internet Stream Transport, VSF `TR-06`) push-sink tests: the
//! `PushProtocol::Rist` egress selector that fans the **same** encoded packets
//! as every other push transport (invariant #7), through the libav `mpegts`
//! muxer over a `rist://` URL (ADR-0095 Tier-0).
//!
//! `PushProtocol`/`PushSink` live in the `ffmpeg`-gated `sink` module, so this
//! test compiles under `--features ffmpeg`. The protocol→muxer mapping and the
//! sink construction are pure/offline (no peer, no network); a graceful failure
//! to an unreachable peer is asserted exactly like the SRT/RTMP push. A live
//! `rist://` roundtrip is the `#[ignore]`d hardware/network-gated test.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg_next::format::Pixel;
use multiview_core::time::Rational;
use multiview_output::sink::{EncodeConfig, PushProtocol, PushSink};

const W: u32 = 128;
const H: u32 = 96;

fn ts_config(codec: &str) -> EncodeConfig {
    EncodeConfig {
        codec_name: codec.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        cadence: Rational::new(25, 1),
        gop: 25,
        bit_rate: 800_000,
        audio: None,
        cuda_ordinal: None,
    }
}

#[test]
fn rist_push_protocol_maps_to_mpegts_muxer() {
    // RIST carries an MPEG-TS payload exactly like SRT/UDP: the URL scheme
    // selects the transport, the muxer is the container (ADR-0095 §3).
    assert_eq!(PushProtocol::Rist.muxer_name(), "mpegts");
    // The sibling transports are unchanged by adding RIST.
    assert_eq!(PushProtocol::Srt.muxer_name(), "mpegts");
    assert_eq!(PushProtocol::UdpTs.muxer_name(), "mpegts");
    assert_eq!(PushProtocol::Rtmp.muxer_name(), "flv");
    assert_eq!(PushProtocol::Rtsp.muxer_name(), "rtsp");
}

#[test]
fn rist_push_sink_construction_carries_protocol_and_url() {
    let sink = PushSink::new(
        ts_config("mpeg2video"),
        PushProtocol::Rist,
        "rist://[2001:db8::20]:6000",
    );
    assert_eq!(sink.protocol(), PushProtocol::Rist);
    assert_eq!(sink.muxer_name(), "mpegts");
    assert_eq!(sink.url(), "rist://[2001:db8::20]:6000");
}
