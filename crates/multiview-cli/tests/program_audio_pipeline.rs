//! End-to-end test that opting into program audio produces a **dual-stream**
//! container — one video stream plus one (silent) AAC audio stream (AUD-4,
//! feature `ffmpeg`).
//!
//! It builds the real libav* pipeline over a minimal multiview, calls
//! [`Pipeline::enable_program_audio`], drives a short bounded run to a file, and
//! `ffprobe`s the produced `program.ts` to confirm it carries exactly ONE video
//! stream and exactly ONE audio stream. The audio is silence (no audio sources
//! are wired in this slice), but it is a real AAC elementary stream, proving the
//! encode-once-mux-many path now fans both the video AND the program-audio
//! packets into the muxer. The video stays exactly N frames for N ticks, so the
//! off-hot-path audio encode never falters the output (invariant #1). No
//! tautology: every assertion is against the on-disk artifact.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

use multiview_cli::pipeline::Pipeline;
use multiview_config::MultiviewConfig;

/// Count the elementary streams of `kind` (`"a"` audio, `"v"` video) in `path`,
/// de-duplicated by stream index so the MPEG-TS double-listing (PMT + PES) does
/// not inflate the count.
fn ffprobe_stream_count(path: &Path, kind: &str) -> usize {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            kind,
            "-show_entries",
            "stream=index",
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
    let mut indices: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(',').to_owned())
        .filter(|l| !l.is_empty())
        .collect();
    indices.sort_unstable();
    indices.dedup();
    indices.len()
}

/// The AAC `codec_name` of the first audio stream of `path`.
fn ffprobe_audio_codec(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    assert!(out.status.success(), "ffprobe audio codec failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .map(|l| l.trim_end_matches(','))
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// A 1x1 config: a single built-in `test` source plus one HLS output (which
/// also anchors the self-contained `program.ts`), requesting the LGPL
/// `mpeg2video` video codec.
fn config_text(out_playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = 640
height = 360
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist = out_playlist.display(),
    )
}

#[tokio::test]
async fn program_audio_produces_a_dual_stream_container() {
    const TICKS: u64 = 30; // 1.2 s @ 25 fps — short but enough to ffprobe.

    let dir = tempfile::tempdir().expect("tempdir");
    let out_dir = dir.path().join("out");
    let playlist = out_dir.join("index.m3u8");
    let toml = config_text(&playlist);

    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");

    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");
    pipeline.enable_program_audio();
    let report = pipeline
        .run_for(TICKS)
        .await
        .expect("bounded real run with program audio");

    // Invariant #1: the program-audio encode runs OFF the hot path, so the video
    // is still exactly N frames for N ticks and never faltered.
    assert_eq!(report.frames, TICKS, "N ticks must produce N frames");
    assert!(
        !report.faltered,
        "program-audio encode must not falter the output"
    );

    let program = out_dir.join("program.ts");
    assert!(program.exists(), "no program.ts written");
    assert!(
        program.metadata().expect("stat program").len() > 0,
        "program.ts is empty"
    );

    // The dual-stream proof: exactly one video stream AND exactly one audio
    // stream, the audio being a real AAC elementary stream.
    assert_eq!(
        ffprobe_stream_count(&program, "v"),
        1,
        "program.ts must carry exactly one video stream"
    );
    assert_eq!(
        ffprobe_stream_count(&program, "a"),
        1,
        "program.ts must carry exactly one audio stream"
    );
    assert_eq!(
        ffprobe_audio_codec(&program),
        "aac",
        "the program-audio stream must be AAC"
    );
}
