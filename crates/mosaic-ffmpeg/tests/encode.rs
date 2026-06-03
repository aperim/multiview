//! Encoder wrapper tests: configuration, the LGPL-codec contract, and the
//! send-frame/receive-packet loop on synthetic frames (no media file needed).
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use ffmpeg::format::Pixel;
use ffmpeg::util::frame::Video;
use ffmpeg_next as ffmpeg;
use mosaic_core::time::Rational;
use mosaic_ffmpeg::{VideoEncodeTarget, VideoEncoder};

const W: u32 = 64;
const H: u32 = 48;

fn target(codec: &str) -> VideoEncodeTarget {
    VideoEncodeTarget {
        codec_name: codec.to_owned(),
        width: W,
        height: H,
        format: Pixel::YUV420P,
        time_base: Rational::new(1, 25),
        bit_rate: 500_000,
        gop: 12,
    }
}

fn gray_yuv420p(pts: i64) -> Video {
    let mut frame = Video::new(Pixel::YUV420P, W, H);
    for p in 0..frame.planes() {
        for byte in frame.data_mut(p).iter_mut() {
            *byte = 128;
        }
    }
    frame.set_pts(Some(pts));
    frame
}

#[test]
fn opens_lgpl_mpeg2video_encoder_and_reports_time_base() {
    let enc = VideoEncoder::new(&target("mpeg2video")).expect("open mpeg2video");
    // The exact time-base must survive configuration (invariant #3).
    assert_eq!(enc.time_base(), Rational::new(1, 25));
}

#[test]
fn unknown_codec_is_a_typed_error() {
    // A codec that does not exist must surface CodecNotFound — never a panic.
    match VideoEncoder::new(&target("this_codec_does_not_exist")) {
        Ok(_) => panic!("a nonexistent codec must not open"),
        Err(err) => assert!(
            err.to_string().contains("not found"),
            "typed CodecNotFound error, got: {err}"
        ),
    }
}

#[test]
fn encodes_frames_into_packets_with_carried_timestamps() {
    let mut enc = VideoEncoder::new(&target("mpeg2video")).expect("open encoder");

    let mut packet_count = 0_u32;
    let mut saw_pts = false;
    for tick in 0..10_i64 {
        // Re-stamp from the tick counter (the output-clock contract).
        let frame = gray_yuv420p(tick);
        enc.send_frame(&frame).expect("send frame");
        while let Some(pkt) = enc.receive_packet().expect("recv packet") {
            packet_count += 1;
            if pkt.pts().is_some() {
                saw_pts = true;
            }
        }
    }
    enc.send_eof().expect("flush");
    while let Some(pkt) = enc.receive_packet().expect("drain") {
        packet_count += 1;
        if pkt.pts().is_some() {
            saw_pts = true;
        }
    }

    assert!(packet_count > 0, "encoder produced packets");
    assert!(saw_pts, "encoded packets carry presentation timestamps");
}

#[test]
fn video_encoder_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<VideoEncoder>();
}
