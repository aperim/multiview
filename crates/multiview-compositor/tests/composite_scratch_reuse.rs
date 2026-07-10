//! Persistent-backend efficiency contract for the CPU reference compositor
//! (efficiency findings #5 + #6): the transfer LUTs and the per-band composite
//! scratch are owned by the persistent [`RunBackend`] and **reused across output
//! ticks**, not rebuilt/reallocated every tick.
//!
//! Both are per-tick costs on the protected output core (invariant #1 runs one
//! composite per tick, forever):
//!
//! * **#5 — transfer LUTs.** Rebuilding the EOTF+OETF tables is thousands of
//!   `pow`/`exp` evaluations per tick (a BT.709 pair alone samples 6144 + 4096
//!   nodes; a PQ EOTF is 12288). A steady run with a stable set of transfer
//!   characteristics must build the LUTs **once** and reuse them.
//! * **#6 — band accumulator.** The tile-driven kernel's premultiplied-linear
//!   accumulator (plus its coverage map) is the frame's largest transient
//!   (~33 MB/tick for a full-cover 1080p canvas). It must come from a pool
//!   allocated at warmup and reused, not `vec!`-allocated every tick.
//!
//! These assertions are deterministic **behavioural counters** on the backend,
//! not wall-clock timings, so they hold on any runner (no GPU, no timing race).
//! The *bit-exactness* of the reuse is pinned elsewhere — `backend_select.rs`
//! (`cpu_backend_matches_free_function_byte_for_byte`) proves the pooled backend
//! equals the free-function oracle, and `composite_tile_driven.rs` proves the
//! tile-driven kernel equals the pixel-driven oracle. Here we additionally assert
//! the output is byte-**stable** across reused ticks: reuse must never leak stale
//! accumulator or coverage state into a later frame.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_compositor::backend::RunBackend;
use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image, Tile};
use multiview_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

/// Number of steady-state ticks to run after warmup.
const TICKS: usize = 16;

/// A flat NV12 tile (mid-gray luma, neutral chroma) of the given even geometry,
/// tagged with `transfer` (primaries/matrix/range fixed at BT.709 limited so the
/// only axis that varies between tiles is the transfer characteristic).
fn flat_tile(w: u32, h: u32, transfer: TransferCharacteristic) -> Nv12Image {
    let pixels =
        usize::try_from(w).expect("width fits usize") * usize::try_from(h).expect("height fits");
    let y = vec![128u8; pixels];
    let uv = vec![128u8; pixels / 2];
    let color = ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    };
    Nv12Image::new(w, h, y, uv, color).expect("flat tile geometry")
}

/// A canvas below `PARALLEL_PIXEL_THRESHOLD` (256×256) → the serial band path.
const SERIAL: (u32, u32) = (128, 128);
/// A canvas above the threshold → the parallel multi-band path.
const PARALLEL: (u32, u32) = (512, 512);

#[test]
fn transfer_luts_built_once_across_ticks_serial_path() {
    let backend = RunBackend::cpu();
    let img = flat_tile(64, 64, TransferCharacteristic::Bt709);
    let tiles = [Tile::placed(&img, 0, 0, 1.0)];
    for _ in 0..TICKS {
        backend
            .composite(
                SERIAL.0,
                SERIAL.1,
                CanvasColor::default(),
                LinearRgba::TRANSPARENT,
                &tiles,
            )
            .expect("composite");
    }
    assert_eq!(
        backend.lut_build_count(),
        1,
        "transfer LUTs must be built once and reused every tick (serial path)"
    );
}

#[test]
fn transfer_luts_built_once_across_ticks_parallel_path() {
    let backend = RunBackend::cpu();
    let img = flat_tile(64, 64, TransferCharacteristic::Bt709);
    let tiles = [Tile::placed(&img, 0, 0, 1.0)];
    for _ in 0..TICKS {
        backend
            .composite(
                PARALLEL.0,
                PARALLEL.1,
                CanvasColor::default(),
                LinearRgba::TRANSPARENT,
                &tiles,
            )
            .expect("composite");
    }
    assert_eq!(
        backend.lut_build_count(),
        1,
        "transfer LUTs must be built once and reused every tick (parallel path)"
    );
}

#[test]
fn changing_transfer_set_rebuilds_luts_then_reuses() {
    // Memoization must be keyed on the SET of transfer characteristics in play
    // (canvas + tiles): a stable set reuses; a changed set invalidates + rebuilds.
    let backend = RunBackend::cpu();
    let bt709 = flat_tile(64, 64, TransferCharacteristic::Bt709);
    let pq = flat_tile(64, 64, TransferCharacteristic::Pq);
    let set_a = [Tile::placed(&bt709, 0, 0, 1.0)]; // set {BT.709} (canvas is BT.709 too)
    let set_b = [Tile::placed(&pq, 0, 0, 1.0)]; // set {BT.709, PQ}
    let bg = LinearRgba::TRANSPARENT;
    let canvas = CanvasColor::default();

    backend
        .composite(SERIAL.0, SERIAL.1, canvas, bg, &set_a)
        .expect("composite a");
    backend
        .composite(SERIAL.0, SERIAL.1, canvas, bg, &set_a)
        .expect("composite a again");
    assert_eq!(
        backend.lut_build_count(),
        1,
        "a stable transfer set must reuse the cached LUTs"
    );

    backend
        .composite(SERIAL.0, SERIAL.1, canvas, bg, &set_b)
        .expect("composite b");
    assert_eq!(
        backend.lut_build_count(),
        2,
        "a changed transfer set must invalidate and rebuild the LUTs"
    );
    backend
        .composite(SERIAL.0, SERIAL.1, canvas, bg, &set_b)
        .expect("composite b again");
    assert_eq!(
        backend.lut_build_count(),
        2,
        "the new set must then reuse, not rebuild"
    );

    backend
        .composite(SERIAL.0, SERIAL.1, canvas, bg, &set_a)
        .expect("composite a once more");
    assert_eq!(
        backend.lut_build_count(),
        3,
        "returning to the first set is a set change → one more rebuild"
    );
}

#[test]
fn band_scratch_reused_after_warmup_serial_path() {
    let backend = RunBackend::cpu();
    let img = flat_tile(96, 96, TransferCharacteristic::Bt709);
    let tiles = [Tile::placed(&img, 8, 8, 1.0)];
    // First tick allocates the pool at this canvas/span; subsequent ticks reuse.
    backend
        .composite(
            SERIAL.0,
            SERIAL.1,
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
            &tiles,
        )
        .expect("warmup composite");
    let warm = backend.scratch_alloc_count();
    for _ in 0..TICKS {
        backend
            .composite(
                SERIAL.0,
                SERIAL.1,
                CanvasColor::default(),
                LinearRgba::TRANSPARENT,
                &tiles,
            )
            .expect("steady composite");
    }
    assert_eq!(
        backend.scratch_alloc_count(),
        warm,
        "band scratch must be reused after warmup, not reallocated per tick (serial)"
    );
}

#[test]
fn band_scratch_reused_after_warmup_parallel_path() {
    let backend = RunBackend::cpu();
    let img = flat_tile(128, 128, TransferCharacteristic::Bt709);
    let tiles = [Tile::placed(&img, 0, 0, 1.0)];
    backend
        .composite(
            PARALLEL.0,
            PARALLEL.1,
            CanvasColor::default(),
            LinearRgba::TRANSPARENT,
            &tiles,
        )
        .expect("warmup composite");
    let warm = backend.scratch_alloc_count();
    for _ in 0..TICKS {
        backend
            .composite(
                PARALLEL.0,
                PARALLEL.1,
                CanvasColor::default(),
                LinearRgba::TRANSPARENT,
                &tiles,
            )
            .expect("steady composite");
    }
    assert_eq!(
        backend.scratch_alloc_count(),
        warm,
        "band scratch must be reused after warmup, not reallocated per tick (parallel)"
    );
}

#[test]
fn reused_ticks_are_byte_stable() {
    // Reuse must not corrupt output: an offset, partial-opacity tile makes the
    // covered span smaller than the band (exercising the coverage sentinel and
    // the background-outside-span fill), and every reused tick must reproduce the
    // first frame byte-for-byte.
    let backend = RunBackend::cpu();
    let img = flat_tile(96, 96, TransferCharacteristic::Bt709);
    let tiles = [Tile::placed(&img, 24, 24, 0.6)];
    let bg = LinearRgba::opaque(0.1, 0.2, 0.3);
    let first = backend
        .composite(PARALLEL.0, PARALLEL.1, CanvasColor::default(), bg, &tiles)
        .expect("first composite");
    for _ in 0..TICKS {
        let again = backend
            .composite(PARALLEL.0, PARALLEL.1, CanvasColor::default(), bg, &tiles)
            .expect("reused composite");
        assert_eq!(
            again.y_plane(),
            first.y_plane(),
            "reused-tick Y plane must be byte-identical to the first frame"
        );
        assert_eq!(
            again.uv_plane(),
            first.uv_plane(),
            "reused-tick UV plane must be byte-identical to the first frame"
        );
    }
}
