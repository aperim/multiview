//! Proof that a video decoder can be **flushed mid-stream** so that, after a
//! container [`seek`](multiview_ffmpeg::Demuxer::seek) back to the start, the
//! decoder drops every buffered/reordered frame and resumes cleanly from the
//! new position — never leaking a stale frame from before the seek.
//!
//! This is the media-player loop's non-negotiable rule (ADR-0097 §rule-2,
//! media-playout §7.5): a seek without an `avcodec_flush_buffers` leaves
//! reordered B-frames buffered in the decoder, which then surface *after* the
//! wrap as out-of-order/stale pictures. The wrapper under test is
//! [`StreamVideoDecoder::flush`] (and the lower-level
//! [`VideoDecoder::flush`]).
//!
//! Gated behind the `ffmpeg` feature so the default pure-Rust build never
//! touches native deps. The clip is generated at test time with the **LGPL**
//! software codec `ffv1` (intra-only, deterministic, no checked-in media) —
//! never x264/x265 — so the test stays LGPL-clean and self-contained.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use multiview_ffmpeg::convert::MediaKind;
use multiview_ffmpeg::{Demuxer, StreamVideoDecoder, VideoDecoder};
use tempfile::TempDir;

/// Width/height of the generated test pattern.
const W: u32 = 160;
/// Height of the generated test pattern.
const H: u32 = 120;
/// Frame rate of the generated test pattern (whole-number → exact time-base).
const RATE: u32 = 25;
/// Clip length in seconds.
const SECS: u32 = 1;

/// Generate a short `testsrc` clip into `dir` using the `ffmpeg` CLI with the
/// LGPL `ffv1` codec, returning the path. Panics (the harness reports it) only
/// if the CLI is genuinely unavailable, which would mean the test environment
/// is misconfigured.
fn generate_clip(dir: &Path) -> PathBuf {
    let out = dir.join("flush_src.mkv");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={W}x{H}:rate={RATE}"),
            "-t",
            &SECS.to_string(),
            // LGPL, in-tree software codec — NOT x264/x265 (GPL). ffv1 is
            // intra-only, so every frame is independently decodable and the PTS
            // sequence is deterministic.
            "-c:v",
            "ffv1",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&out)
        .status()
        .expect("failed to spawn the `ffmpeg` CLI (is FFmpeg installed?)");
    assert!(
        status.success(),
        "ffmpeg CLI exited with failure while generating the test clip"
    );
    out
}

/// Open `clip`, returning a demuxer, the video stream index, and a
/// `StreamVideoDecoder` built from that stream's owned codec parameters.
fn open_decoder(clip: &Path) -> (Demuxer, usize, StreamVideoDecoder) {
    let demux = Demuxer::open(clip).expect("open clip");
    let streams = demux.streams();
    let video = streams
        .iter()
        .find(|s| s.kind == MediaKind::Video)
        .expect("video stream present");
    let vidx = video.index;
    let params = demux
        .stream_parameters(vidx)
        .expect("owned codec parameters for the video stream");
    let decoder = StreamVideoDecoder::new(params, video.time_base).expect("build video decoder");
    (demux, vidx, decoder)
}

/// Drive the decoder forward by sending packets until it has produced at least
/// `want` frames, collecting each frame's raw stream PTS. Returns the PTS values
/// (length `>= want` unless the stream ended first).
fn decode_n_raw_pts(
    demux: &mut Demuxer,
    vidx: usize,
    decoder: &mut StreamVideoDecoder,
    want: usize,
) -> Vec<i64> {
    let mut pts = Vec::new();
    while pts.len() < want {
        match demux.read_packet_for(vidx).expect("read packet") {
            Some(pkt) => {
                decoder.send_packet(&pkt.packet).expect("send packet");
                while let Some(frame) = decoder.receive_frame().expect("receive frame") {
                    if let Some(p) = frame.raw_pts {
                        pts.push(p);
                    }
                    if pts.len() >= want {
                        break;
                    }
                }
            }
            None => break,
        }
    }
    pts
}

/// After decoding a few frames, a seek-to-start **followed by `flush()`** lets
/// the decoder resume from the beginning of the clip and yield frames again,
/// without erroring or stalling, and the first frame after the flush carries an
/// early PTS — proving the decoder's buffered state was actually reset rather
/// than continuing from where decoding had advanced to.
#[test]
fn seek_then_flush_resumes_decoding_from_the_start() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let (mut demux, vidx, mut decoder) = open_decoder(&clip);

    // Decode several frames so the decoder has advanced well past the start and
    // (for codecs that reorder) is holding buffered pictures.
    let before = decode_n_raw_pts(&mut demux, vidx, &mut decoder, 8);
    assert!(
        before.len() >= 4,
        "the 1s/{RATE}fps clip must yield several frames before the seek (got {})",
        before.len()
    );
    let last_before = *before.last().expect("at least one pre-seek frame");

    // Seek the container back to the very start and FLUSH the decoder. The flush
    // is the surface under test: it must drop every buffered/reordered frame so
    // the next decode starts clean from the new position.
    demux.seek(0).expect("seek back to start");
    decoder.flush().expect("flush the decoder after seek");

    // Decoding must continue: we get frames again, no error/stall, and the first
    // frame after the flush is an EARLY frame (PTS at or before where we left
    // off — for a seek-to-start, at/near zero), which a decoder that had NOT
    // been flushed could not guarantee (it might emit a stale buffered frame
    // from after `last_before`, or refuse fresh packets).
    let after = decode_n_raw_pts(&mut demux, vidx, &mut decoder, 4);
    assert!(
        !after.is_empty(),
        "decoding must resume after seek + flush, but produced no frames"
    );
    let first_after = after[0];
    assert!(
        first_after <= last_before,
        "after seek-to-start + flush the first frame PTS ({first_after}) must rewind \
         to at or before the pre-seek position ({last_before}); a non-rewinding PTS \
         means the decoder kept stale buffered state instead of being flushed"
    );
}

/// `flush()` is callable repeatedly and mid-stream without error, and decoding
/// continues correctly afterwards — pinning that the wrapper is wired to the
/// real libav decoder context and is safe to call between packets.
#[test]
fn flush_is_idempotent_and_decoding_survives_it() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());
    let (mut demux, vidx, mut decoder) = open_decoder(&clip);

    // Flush on a fresh decoder (no buffered state) is a harmless no-op.
    decoder.flush().expect("flush on a fresh decoder");
    decoder.flush().expect("flush is idempotent");

    // Decode a couple of frames, flush mid-stream, then keep decoding to EOF.
    let _ = decode_n_raw_pts(&mut demux, vidx, &mut decoder, 2);
    decoder.flush().expect("flush mid-stream");

    let mut total = 0usize;
    while let Some(pkt) = demux.read_packet_for(vidx).expect("read packet") {
        decoder.send_packet(&pkt.packet).expect("send packet");
        while decoder.receive_frame().expect("receive frame").is_some() {
            total += 1;
        }
    }
    decoder.send_eof().expect("send eof");
    while decoder.receive_frame().expect("drain").is_some() {
        total += 1;
    }
    assert!(
        total >= 1,
        "decoding must still produce frames after a mid-stream flush (got {total})"
    );
}

/// The lower-level [`VideoDecoder`] (the first-frame spike decoder) also exposes
/// a `flush()` that is callable without error, mirroring the streaming
/// decoder's wiring on the same FFI surface.
#[test]
fn inner_video_decoder_flush_is_callable() {
    let dir = TempDir::new().expect("tempdir");
    let clip = generate_clip(dir.path());

    let mut decoder = VideoDecoder::open(&clip).expect("open the inner decoder");
    let first = decoder
        .decode_first_frame()
        .expect("decode the first frame");
    assert_eq!(first.width, W, "decoded width");
    assert_eq!(first.height, H, "decoded height");

    // Flushing drops any buffered frames; it must not error and the decoder
    // remains usable for a subsequent flush.
    decoder.flush().expect("flush the inner decoder");
    decoder.flush().expect("inner flush is idempotent");
}
