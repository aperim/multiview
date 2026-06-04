//! End-to-end test of the **real** libav\* `multiview run` pipeline (the `ffmpeg`
//! feature).
//!
//! This is the integration counterpart to the headless smoke: it generates a
//! real input clip with the `ffmpeg` CLI, writes a small 2x2 config (canvas
//! 1280x720@25, a file source + test patterns, an HLS output), builds the
//! [`RealPipeline`], drives a bounded run, and then **`ffprobe`s the produced
//! file** to confirm it is a real, playable multiview: the right codec/resolution,
//! the right frame count, and a decodable HLS segment. No tautologies — every
//! assertion is against a real on-disk artifact.
//!
//! Licensing: the test asks for `mpeg2video` (LGPL, in-tree) so it passes under
//! the plain `ffmpeg` feature with no GPL escalation.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use multiview_cli::pipeline::RealPipeline;
use multiview_config::MultiviewConfig;

/// Generate a 2-second `testsrc` clip (LGPL `mpeg2video`) for the file source.
fn generate_clip(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=2",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-f",
            "mpegts",
        ])
        .arg(path)
        .status()
        .expect("spawn ffmpeg CLI to generate the input clip");
    assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    assert!(path.exists(), "input clip was not written");
}

/// Run `ffprobe` and return the single requested stream entry value for the
/// first video stream of `path`.
fn ffprobe_v(path: &Path, entry: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            &format!("stream={entry}"),
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed for {}",
        path.display()
    );
    // mpegts may list the stream twice; take the first non-empty line.
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_end_matches(',')
        .to_owned()
}

/// Count decodable video frames in `path` via `ffprobe -count_frames`.
fn ffprobe_frame_count(path: &Path) -> u64 {
    let value = {
        let out = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-count_frames",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("spawn ffprobe -count_frames");
        assert!(out.status.success(), "ffprobe -count_frames failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().trim_end_matches(','))
            .find(|l| !l.is_empty() && *l != "N/A")
            .unwrap_or("0")
            .to_owned()
    };
    value.parse().expect("frame count is an integer")
}

/// A small 2x2 config: a real file source, two built-in test patterns, a fourth
/// file source, and an HLS output requesting the LGPL `mpeg2video` codec.
fn config_text(clip: &Path, out_playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 1280
height = 720
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "file"
path = "{clip}"
[[sources]]
id = "in_b"
kind = "test"
[[sources]]
id = "in_c"
kind = "test"
[[sources]]
id = "in_d"
kind = "file"
path = "{clip}"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[cells]]
id = "cell_b"
area = "b"
[cells.source]
input_id = "in_b"
[[cells]]
id = "cell_c"
area = "c"
[cells.source]
input_id = "in_c"
[[cells]]
id = "cell_d"
area = "d"
[cells.source]
input_id = "in_d"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        clip = clip.display(),
        playlist = out_playlist.display(),
    )
}

#[tokio::test]
async fn real_pipeline_produces_a_playable_multiview_and_hls() {
    const TICKS: u64 = 50; // 2 s @ 25 fps

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = RealPipeline::build(&config).expect("build real pipeline");
    assert_eq!(pipeline.source_count(), 4, "config wires four sources");
    // The LGPL-clean default encoder is mpeg2video (no GPL escalation in this
    // feature set).
    assert_eq!(pipeline.encoder_name(), "mpeg2video");

    let report = pipeline.run_for(TICKS).await.expect("bounded real run");

    // Invariant #1: exactly N frames for N ticks, never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(!report.faltered, "output must never falter (invariant #1)");
    assert_eq!(report.canvas_width, 1280);
    assert_eq!(report.canvas_height, 720);

    // The single-file program is derived beside the playlist (encode-once).
    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert!(
        program.metadata().expect("stat program").len() > 0,
        "program.ts is empty"
    );

    // ffprobe the produced file: a real, playable mpeg2video 1280x720 multiview
    // with exactly the right number of frames.
    assert_eq!(ffprobe_v(&program, "codec_name"), "mpeg2video");
    assert_eq!(ffprobe_v(&program, "width"), "1280");
    assert_eq!(ffprobe_v(&program, "height"), "720");
    assert_eq!(ffprobe_v(&program, "r_frame_rate"), "25/1");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts must decode to exactly N frames"
    );

    // The HLS playlist exists, references segments, and the first segment is an
    // independently decodable file.
    assert!(playlist.exists(), "no HLS playlist written");
    let playlist_text = std::fs::read_to_string(&playlist).expect("read playlist");
    assert!(playlist_text.starts_with("#EXTM3U"));
    assert!(playlist_text.contains("#EXT-X-ENDLIST"));
    assert!(playlist_text.contains("seg0.ts"));

    let seg0 = out_dir.join("seg0.ts");
    assert!(seg0.exists(), "first HLS segment not written");
    assert!(
        ffprobe_frame_count(&seg0) > 0,
        "first HLS segment did not decode any frame"
    );
}

#[tokio::test]
async fn real_pipeline_holds_output_when_a_source_runs_out() {
    // The file source is a 2 s clip (50 frames). Driving 75 ticks must STILL
    // produce 75 frames: a source past its last frame holds its last-good frame
    // rather than stalling the output clock (invariant #1).
    const TICKS: u64 = 75;

    let dir = tempfile::tempdir().expect("tempdir");
    let clip = dir.path().join("in.ts");
    generate_clip(&clip);

    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&clip, &playlist);
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = RealPipeline::build(&config).expect("build real pipeline");
    let report = pipeline.run_for(TICKS).await.expect("bounded real run");

    assert_eq!(
        report.frames, TICKS,
        "output is independent of input exhaustion (invariant #1)"
    );
    assert!(!report.faltered);

    let program = out_dir.join("program.ts");
    assert_eq!(
        ffprobe_frame_count(&program),
        TICKS,
        "program.ts decodes to N frames even though the file source had fewer"
    );
}
