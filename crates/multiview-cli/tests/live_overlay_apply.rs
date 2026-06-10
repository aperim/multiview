//! Live overlay add / edit / remove on the running engine (ADR-W021).
//!
//! The command drain applies `UpsertOverlay`/`RemoveOverlay` at the frame
//! boundary as pure data mutation: it upserts/removes the document in the
//! working-config mirror and publishes the full set (with a bumped generation)
//! into the lock-free [`OverlayApplySlot`] the bake consumer re-derives from.
//! These tests prove the drain slice end-to-end on the software engine:
//!
//! * an upsert publishes the new set at the frame boundary (generation bump);
//! * an edit under the same id replaces the entry — never duplicates it;
//! * a remove drops the entry; removing an unknown id is a no-op publish-wise;
//! * a drain without the seam holds the command (warned, never a panic);
//! * a continuous upsert/remove churn flood cannot stall the clock or skip a
//!   frame (invariants #1 + #10) — the soak gate.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_cli::control::{command_drain, command_drain_with_live_overlays};
use multiview_cli::live_overlays::overlay_apply_slot;
use multiview_cli::run::SoftwareEngine;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_control::{command_bus, Command, EngineStateSnapshot, OperationId};
use multiview_engine::{CompositorDrive, EnginePublisher};
use multiview_events::Event;
use multiview_framestore::TileStore;

/// A 2-cell config with two declared bars sources bound to the cells.
fn two_cell_config() -> MultiviewConfig {
    let doc = r##"schema_version = 1
[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr"]
areas = ["a b"]
[[sources]]
id = "in_a"
kind = "bars"
[[sources]]
id = "in_b"
kind = "bars"
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
[[outputs]]
kind = "hls"
path = "/tmp/live-overlay-apply.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##;
    MultiviewConfig::load_from_toml(doc).expect("parse two-cell config")
}

/// A real `CompositorDrive` over the config's solved layout (the drain's
/// frame-boundary target).
fn test_drive(config: &MultiviewConfig) -> CompositorDrive<Nv12Image> {
    let layout = config.solve_layout().expect("solve layout");
    let canvas_color = CanvasColor::default();
    let nosignal = Nv12Image::solid(
        config.canvas.width,
        config.canvas.height,
        16,
        128,
        128,
        canvas_color.output_tag(),
    )
    .expect("nosignal card");
    let mut stores = std::collections::HashMap::new();
    for source in &config.sources {
        stores.insert(
            source.id.clone(),
            Arc::new(TileStore::<Nv12Image>::with_defaults(source.id.clone())),
        );
    }
    CompositorDrive::new(
        Arc::new(layout),
        stores,
        nosignal,
        canvas_color,
        LinearRgba::opaque(0.0, 0.0, 0.0),
    )
    .expect("build drive")
}

/// Parse a validated `multiview_config::Overlay` from JSON (the same shape the
/// typed overlays route stores after ADR-W015 validation).
fn overlay_doc(json: serde_json::Value) -> multiview_config::Overlay {
    serde_json::from_value(json).expect("valid overlay document")
}

/// An analog wall-clock overlay document centred at (`x`, `y`).
fn analog_clock(id: &str, x: i64, y: i64) -> multiview_config::Overlay {
    overlay_doc(serde_json::json!({
        "id": id, "kind": "clock", "target": "canvas",
        "face": "analog", "x": x, "y": y, "radius": 16
    }))
}

#[tokio::test]
async fn upsert_overlay_publishes_the_new_set_at_the_frame_boundary() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);

    assert_eq!(slot.load().generation(), 0, "the seeded slot is generation 0");
    assert!(slot.load().overlays().is_empty(), "boot config has no overlays");

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 200, 120)),
        })
        .expect("submit upsert");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(set.generation(), 1, "the apply bumps the generation");
    assert_eq!(set.overlays().len(), 1);
    assert_eq!(set.overlays()[0].id, "clk");
    assert_eq!(set.overlays()[0].kind, "clock");

    // EDIT: an upsert under the same id REPLACES the entry (never duplicates).
    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 64, 64)),
        })
        .expect("submit edit");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(set.generation(), 2);
    assert_eq!(set.overlays().len(), 1, "an edit must not duplicate the id");
    assert_eq!(
        set.overlays()[0].params.get("x"),
        Some(&serde_json::json!(64)),
        "the published set carries the NEW params"
    );
}

#[tokio::test]
async fn remove_overlay_drops_the_entry_from_the_published_set() {
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let slot = overlay_apply_slot(config.overlays.clone());
    let mut drain = command_drain_with_live_overlays(
        command_rx,
        config.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );
    let mut drive = test_drive(&config);

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 200, 120)),
        })
        .expect("submit upsert");
    drain(&mut drive);
    assert_eq!(slot.load().overlays().len(), 1);

    sender
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: "clk".to_owned(),
        })
        .expect("submit remove");
    drain(&mut drive);

    let set = slot.load();
    assert_eq!(set.generation(), 2, "the remove publishes a new generation");
    assert!(set.overlays().is_empty(), "the entry is gone from the set");

    // Removing an UNKNOWN id is a logged no-op: nothing new is published.
    sender
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: "ghost".to_owned(),
        })
        .expect("submit ghost remove");
    drain(&mut drive);
    assert_eq!(
        slot.load().generation(),
        2,
        "an unknown-id remove publishes nothing (no spurious re-derive)"
    );
}

#[tokio::test]
async fn overlay_commands_without_a_seam_are_held_without_panicking() {
    // The plain drain (software path without the seam wired) holds the
    // commands — warned, never a panic, and the clock-side apply still runs.
    let config = two_cell_config();
    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(16));
    let (sender, command_rx) = command_bus(16);
    let mut drain = command_drain(command_rx, config.clone(), Arc::clone(&publisher));
    let mut drive = test_drive(&config);

    sender
        .try_submit(Command::UpsertOverlay {
            op: OperationId::new(),
            overlay: Box::new(analog_clock("clk", 200, 120)),
        })
        .expect("submit upsert");
    sender
        .try_submit(Command::RemoveOverlay {
            op: OperationId::new(),
            id: "clk".to_owned(),
        })
        .expect("submit remove");
    drain(&mut drive);
}

/// SOAK (invariants #1 + #10): a continuous full-speed flood of live overlay
/// churn — upserts at varying placements and removes — must never stall the
/// output clock or skip a frame: exactly N frames for N ticks, never faltered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_overlay_churn_flood_never_falters_the_output_clock() {
    use multiview_engine::{CooperativePacer, ManualTimeSource};
    use std::sync::atomic::{AtomicBool, Ordering};

    const TICKS: u64 = 240;

    let cfg = two_cell_config();
    let mut engine = SoftwareEngine::build(&cfg).expect("build software engine");
    let (tx, rx) = command_bus(64);
    let slot = overlay_apply_slot(cfg.overlays.clone());

    // Background flooder: live overlay churn at full speed for the whole run.
    // A full bus just sheds the submit (inv #10).
    let stop_flood = Arc::new(AtomicBool::new(false));
    let flooder = {
        let stop_flood = Arc::clone(&stop_flood);
        std::thread::spawn(move || {
            let mut n: i64 = 0;
            while !stop_flood.load(Ordering::Relaxed) {
                let id = format!("churn{}", n % 4);
                let _ = tx.try_submit(Command::UpsertOverlay {
                    op: OperationId::new(),
                    overlay: Box::new(
                        serde_json::from_value(serde_json::json!({
                            "id": id, "kind": "clock", "target": "canvas",
                            "face": "analog", "x": 8 + (n % 48), "y": 8 + (n % 48),
                            "radius": 8
                        }))
                        .expect("valid churn overlay"),
                    ),
                });
                let _ = tx.try_submit(Command::RemoveOverlay {
                    op: OperationId::new(),
                    id,
                });
                n = n.wrapping_add(1);
            }
        })
    };

    let publisher = Arc::new(EnginePublisher::<EngineStateSnapshot, Event>::new(8));
    let drain = command_drain_with_live_overlays(
        rx,
        cfg.clone(),
        Arc::clone(&publisher),
        Arc::clone(&slot),
    );

    let time = Arc::new(ManualTimeSource::new());
    let report = engine
        .run_for_with_control(Arc::clone(&time), CooperativePacer, TICKS, drain)
        .await
        .expect("software run under live-overlay churn");

    stop_flood.store(true, Ordering::Relaxed);
    let _ = flooder.join();

    assert_eq!(
        report.frames, TICKS,
        "live-overlay churn must still produce exactly N frames for N ticks"
    );
    assert!(
        !report.faltered,
        "live-overlay churn must never falter the output clock (invariants #1 + #10)"
    );
    assert!(
        slot.load().generation() > 0,
        "the churn actually applied (the slot advanced past the boot set)"
    );
}
