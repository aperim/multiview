//! Node pipeline wiring tests (DEV-B5 / ADR-0045), `ffmpeg` + `display-kms`:
//! the lowered node document **builds** the standard full pipeline without
//! touching hardware (display sinks light at run start, not at build), and
//! the node's hotplug polling cadence threads into the pipeline. Real
//! scanout/ingest stay hardware-validated (the t630 leg).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;
use std::time::Duration;

use multiview_cli::pipeline::Pipeline;
use multiview_cli::preview::program_slot;
use multiview_compositor::pipeline::Nv12Image;
use multiview_config::node::NodeConfig;
use multiview_config::Output;
use multiview_control::EngineStateSnapshot;
use multiview_engine::{CompositorDrive, EnginePublisher, StopSignal};
use multiview_events::Event;

const NODE_TOML: &str = r#"
[ingest]
kind = "rtsp"
url = "rtsp://[2001:db8::10]:8554/program"

[[displays]]
connector = "HDMI-A-1"
audio = true

[hotplug]
poll_secs = 3
"#;

#[test]
fn a_lowered_node_document_builds_the_standard_pipeline_without_hardware() {
    let node = NodeConfig::load_from_toml(NODE_TOML).expect("parses");
    let lowered = node.to_multiview_config().expect("lowers");
    // Build = plans only (ingest threads and display sinks start at run
    // start): a display-only document must build hardware-free.
    let pipeline = Pipeline::build(&lowered).expect("the node pipeline builds");
    assert_eq!(pipeline.source_count(), 1, "one supervised ingest");
    let cadence = pipeline.cadence();
    assert_eq!(
        (cadence.num, cadence.den),
        (60, 1),
        "the default node canvas cadence"
    );
}

#[test]
fn the_node_hotplug_polling_cadence_threads_into_the_pipeline() {
    let node = NodeConfig::load_from_toml(NODE_TOML).expect("parses");
    let lowered = node.to_multiview_config().expect("lowers");
    let mut pipeline = Pipeline::build(&lowered).expect("builds");
    assert_eq!(
        pipeline.display_hotplug_poll(),
        Duration::from_secs(5),
        "the polling fallback defaults to 5 s"
    );
    pipeline.set_display_hotplug_poll(Duration::from_secs(node.hotplug.poll_secs));
    assert_eq!(pipeline.display_hotplug_poll(), Duration::from_secs(3));
}

/// A node document whose feed is DEAD (an RTSP URL nothing serves) and whose
/// `on_loss` slate is the **no-signal card** — the tile must ride the
/// framestore ladder into its down state and composite that card.
const DEAD_FEED_NOSIGNAL_TOML: &str = r#"
[ingest]
kind = "rtsp"
url = "rtsp://[::1]:9/dead"

[[displays]]
connector = "HDMI-A-1"

[canvas]
width = 640
height = 360
fps = "30/1"

[on_loss]
slate = "no_signal"
"#;

/// The adversarial-review MAJOR finding (DEV-B5 F1): a **control-less** run —
/// exactly what `multiview node` lowers to (no control plane; the bounded
/// `run_for` path has no command drain at all) — must composite the
/// configured `on_loss` slate for a downed feed, **not black**. Per-cell
/// `on_loss` must therefore be attached to the drive by the run path itself,
/// never only by the control-plane drain's `set_cell_slates`.
///
/// Proven against the real composited frame the running pipeline publishes
/// into its live program slot (the engine's own output — no tautology). The
/// lowered document's display heads are swapped for an HLS sink ONLY because
/// CI has no DRM device; the path under test (config `on_loss` → `Pipeline` →
/// `CompositorDrive` → compose) is identical for every output transport.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn a_control_less_node_run_composites_the_configured_on_loss_slate_not_black() {
    let node = NodeConfig::load_from_toml(DEAD_FEED_NOSIGNAL_TOML).expect("parses");
    let mut lowered = node.to_multiview_config().expect("lowers");
    let dir = tempfile::tempdir().expect("tempdir");
    let playlist = dir.path().join("node-slate/index.m3u8");
    lowered.outputs = vec![Output::Hls {
        id: None,
        path: playlist.display().to_string(),
        codec: "mpeg2video".to_owned(),
        segment_ms: Some(1_000),
        gpu_pin: None,
        audio: None,
    }];
    lowered
        .validate()
        .expect("the HLS-swapped document validates");

    let mut pipeline = Pipeline::build(&lowered).expect("builds");
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(64));
    let preview_slot = program_slot();
    let stop = StopSignal::new();

    // Capture task: wait for the run to publish its first composited frame
    // (wait-free slot reads), then stop the run.
    let slot = Arc::clone(&preview_slot);
    let stop_for_capture = stop.clone();
    let capture = tokio::spawn(async move {
        let mut frame = None;
        for _ in 0..5_000 {
            if let Some(f) = slot.load_full() {
                frame = Some(f);
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        stop_for_capture.stop();
        frame.expect("the run published a composited frame")
    });

    // The CONTROL-LESS run: a no-op per-frame hook, exactly like the node's
    // signal-driven run (whose drain only services sd_notify) and `run_for`.
    let report = pipeline
        .run_until_serving(
            &stop,
            publisher.as_ref(),
            &preview_slot,
            |_d: &mut CompositorDrive<Nv12Image>| {},
        )
        .await
        .expect("the control-less run serves");
    assert!(!report.faltered, "the output must never falter");

    let frame = capture.await.expect("capture task");
    let (w, h) = (frame.width(), frame.height());
    let (y, cb, cr) = frame.sample(w / 2, h / 2).expect("centre sample");
    // The no-signal card is a flat field across the centre row (a card, not
    // the bars staircase)…
    let uniform = (0..w).all(|x| frame.sample(x, h / 2).map(|s| s.0) == Some(y));
    assert!(
        uniform,
        "the down tile composites a flat card, not the bars staircase"
    );
    // …and it is the chroma-tinted no-signal card, NOT black: a dead feed on a
    // node configured `on_loss = "no_signal"` must show the card the operator
    // chose (ADR-0045: \"shows a local slate exactly as a tile would\").
    assert_ne!(
        y, 16,
        "the down tile must composite the configured no-signal card, not black \
         (luma 16 == the unconfigured default card — the on_loss policy was dropped)"
    );
    assert_ne!(
        (cb, cr),
        (128, 128),
        "the no-signal card is chroma-tinted, not neutral black/grey"
    );
}
