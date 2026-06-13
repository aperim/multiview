//! ADR-W021 — the **program preview surface tells the bake truth** (the
//! on-hardware Defect B reproduction: POST overlay clock → `201` +
//! `X-Multiview-Apply: live` + drain log "overlay applied live", but the
//! program the operator watches showed NO clock).
//!
//! Investigated as-built: the composited canvas — CPU **and** GPU backend —
//! passes through host memory into the off-hot-path bake consumer
//! (`StreamBaker`) before the single encode, so every encoded output (HLS /
//! file / push) carries the baked overlays. The lie was the run's own
//! **program preview slot** (the `WebUI` program monitor): the hot loop
//! published the PRE-bake canvas, so neither live-applied nor config-authored
//! overlays ever appeared on the surface the operator verifies against.
//!
//! This test drives the real pipeline run with the real drain + overlay seam
//! (exactly the binary's wiring), live-applies an analog clock, and asserts
//! the **preview slot** frame shows the clock's bezel ring — one truth across
//! the encoded program and the monitored program.
#![cfg(all(feature = "ffmpeg", feature = "overlay"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    // reason: test-local pixel-probe math (rounding ring coordinates); the
    // production guardrail ban on `as` does not extend to integration tests.
    clippy::as_conversions,
    clippy::cast_possible_truncation
)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_cli::control;
use multiview_cli::live_sources::{shared_stores, LiveSourceHub};
use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::{program_slot, ProgramSlot};
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;

/// Canvas geometry + the clock placement the test applies.
const CANVAS_W: u32 = 320;
const CANVAS_H: u32 = 240;
const CLOCK_CX: i32 = 160;
const CLOCK_CY: i32 = 120;
const CLOCK_RADIUS: f64 = 60.0;

/// Single dark solid cell, HLS output — the program is a near-black canvas
/// until an overlay is baked over it.
fn config_text(out_playlist: &Path) -> String {
    format!(
        r##"
schema_version = 1

[canvas]
width = {CANVAS_W}
height = {CANVAS_H}
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
id = "in_dark"
kind = "solid"
color = "#101010"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_dark"

[[outputs]]
kind = "hls"
path = "{playlist}"
codec = "mpeg2video"
segment_ms = 1000
"##,
        playlist = out_playlist.display(),
    )
}

/// The peak luma sampled on the clock's bezel ring band (8 compass points at
/// `radius - thickness/2` from the centre). The bezel is drawn opaque at
/// ~0.95 grey (Y ≈ 230); the bare program there is ~#101010 (Y ≈ 16).
fn ring_peak_luma(frame: &Nv12Image) -> u8 {
    let y_plane = frame.y_plane();
    let w = usize::try_from(frame.width()).expect("width");
    let mut peak = 0_u8;
    // Mid-ring radius: outer radius minus half the bezel thickness (6% of r).
    let r = CLOCK_RADIUS - (CLOCK_RADIUS * 0.06_f64).max(1.5) / 2.0;
    for step in 0..8_i32 {
        let theta = f64::from(step) * std::f64::consts::FRAC_PI_4;
        let px = f64::from(CLOCK_CX) + r * theta.cos();
        let py = f64::from(CLOCK_CY) + r * theta.sin();
        // Probe a 3x3 neighbourhood around the ideal ring point so raster
        // rounding cannot miss the stroked band.
        for dy in -1..=1_i32 {
            for dx in -1..=1_i32 {
                let (Ok(x), Ok(y)) = (
                    usize::try_from(px.round() as i64 + i64::from(dx)),
                    usize::try_from(py.round() as i64 + i64::from(dy)),
                ) else {
                    continue;
                };
                if let Some(&luma) = y_plane.get(y * w + x) {
                    peak = peak.max(luma);
                }
            }
        }
    }
    peak
}

/// Wait (bounded) until the preview slot holds a frame satisfying `accept`.
fn wait_preview(
    slot: &ProgramSlot,
    deadline: Duration,
    mut accept: impl FnMut(&Nv12Image) -> bool,
) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if let Some(frame) = slot.load_full() {
            if accept(&frame) {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// A live-applied analog clock must appear on the PROGRAM PREVIEW surface —
/// the frame the `WebUI` program monitor serves must be the BAKED program, not
/// the pre-bake canvas (Defect B: header + drain said `live`, the monitored
/// program showed no clock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_applied_overlay_renders_on_the_program_preview() {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_text(&dir.path().join("out/index.m3u8"));
    let config = MultiviewConfig::load_from_toml(&toml).expect("parse config");
    config.validate().expect("config validates");
    let mut pipeline = Pipeline::build(&config).expect("build real pipeline");

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = program_slot();
    let (commands, command_rx) = command_bus(32);
    let stop = StopSignal::new();

    // Exactly the binary's full-pipeline wiring: hub + the drain with the
    // subtitle + overlay seams (`command_drain_with_seams`).
    let hub = LiveSourceHub::start_with_ingest(
        pipeline.stop_registry(),
        shared_stores(pipeline.preview_stores()),
        Some(pipeline.live_ingest_spawner()),
    );
    let mut drain = control::command_drain_with_seams(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        pipeline.subtitle_route_slot(),
        pipeline.overlay_apply_slot(),
        hub.handle(),
    );

    let probe_slot = Arc::clone(&preview_slot);
    let driver_stop = stop.clone();
    let driver = tokio::spawn(async move {
        let (baseline_dark, clock_visible) = tokio::task::block_in_place(|| {
            // Baseline: the program preview serves frames and the ring band is
            // dark (no overlay applied yet).
            let baseline_dark = wait_preview(&probe_slot, Duration::from_secs(10), |frame| {
                ring_peak_luma(frame) < 60
            });

            // Live-apply the analog clock — the operator's exact action.
            let overlay: multiview_config::Overlay = serde_json::from_value(serde_json::json!({
                "id": "clk",
                "kind": "clock",
                "target": "canvas",
                "face": "analog",
                "x": CLOCK_CX,
                "y": CLOCK_CY,
                "radius": CLOCK_RADIUS,
            }))
            .expect("valid clock overlay");
            commands
                .try_submit(Command::UpsertOverlay {
                    op: OperationId::new(),
                    overlay: Box::new(overlay),
                })
                .expect("submit overlay upsert");

            // The clock's bezel ring must reach the PREVIEW surface.
            let clock_visible = wait_preview(&probe_slot, Duration::from_secs(10), |frame| {
                ring_peak_luma(frame) > 150
            });
            (baseline_dark, clock_visible)
        });
        driver_stop.stop();
        (baseline_dark, clock_visible)
    });

    let report = pipeline
        .run_until_serving(
            &stop,
            publisher.as_ref(),
            &preview_slot,
            move |drive: &mut CompositorDrive<Nv12Image>| drain(drive),
        )
        .await
        .expect("serving run with a live overlay apply");
    let (baseline_dark, clock_visible) = driver.await.expect("driver task");
    hub.shutdown();

    assert!(
        baseline_dark,
        "precondition: the bare program preview must be dark on the ring band"
    );
    assert!(
        clock_visible,
        "a live-applied analog clock must render on the PROGRAM PREVIEW surface \
         (the WebUI program monitor) — the preview must serve the BAKED program, \
         not the pre-bake canvas (hw Defect B)"
    );
    assert!(
        !report.faltered,
        "the overlay apply + preview path must never falter the output clock"
    );
}
