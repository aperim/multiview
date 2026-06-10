//! The compositor-drive seam the per-tick alarm driver samples through (M10).
//!
//! The [`AlarmDriver`](multiview_engine::AlarmDriver) routes per **cell id**; the
//! engine drive samples per **source**. These tests pin
//! [`CompositorDrive::sample_cell_luma`] — the production seam that resolves a
//! config cell id to its bound source's *latched* last-good NV12 frame for this
//! tick (the same `read_at` the compositor draws), so the alarm driver analyses
//! exactly the picture on screen — including a re-pointed cell — and never blocks.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::MediaTime;
use multiview_engine::CompositorDrive;
use multiview_framestore::TileStore;

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

fn solid(w: u32, h: u32, y: u8) -> Nv12Image {
    Nv12Image::solid(w, h, y, 128, 128, resolved_color()).unwrap()
}

fn one_cell_layout() -> Layout {
    Layout {
        name: "t".to_owned(),
        canvas: Canvas {
            width: 64,
            height: 36,
            fps_num: 60,
            fps_den: 1,
        },
        cells: vec![Cell {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            z: 0,
            fit: FitMode::Contain,
            source: Some("cam-a".to_owned()),
            ..Cell::default()
        }],
    }
}

fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

#[test]
fn sample_cell_luma_returns_the_bound_sources_latched_frame() {
    let store: Arc<TileStore<Nv12Image>> = Arc::new(TileStore::with_defaults("cam-a"));
    store.publish(solid(64, 36, 33), ms(0));
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), Arc::clone(&store));

    let drive = CompositorDrive::new(
        Arc::new(one_cell_layout()),
        stores,
        solid(64, 36, 16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("tile-1".to_owned())]);

    // The named cell resolves to its bound source's frame (luma 33).
    let frame = drive
        .sample_cell_luma("tile-1", ms(0))
        .expect("named, bound cell has a latched frame");
    assert_eq!(frame.width(), 64);
    assert_eq!(frame.y_plane()[0], 33);

    // An unknown cell id yields None — the alarm driver simply does not advance
    // that probe (never a panic, inv #1).
    assert!(drive.sample_cell_luma("nope", ms(0)).is_none());
}

#[test]
fn sample_cell_luma_is_none_for_a_starved_cell() {
    // A named, bound cell whose source has produced NO frame yet yields None — a
    // starved/absent input cannot drive the alarm engine (inv #1 / #10).
    let store: Arc<TileStore<Nv12Image>> = Arc::new(TileStore::with_defaults("cam-a"));
    let mut stores = HashMap::new();
    stores.insert("cam-a".to_owned(), Arc::clone(&store));

    let drive = CompositorDrive::new(
        Arc::new(one_cell_layout()),
        stores,
        solid(64, 36, 16),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
    .with_cell_ids(vec![Some("tile-1".to_owned())]);

    assert!(drive.sample_cell_luma("tile-1", ms(0)).is_none());
}
