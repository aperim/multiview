//! Equivalence + budget tests for the **tile-driven** CPU composite kernel
//! (the O(pixels × covering-tiles) rewrite of the old O(pixels × all-tiles)
//! kernel; the discovery critic's #1 perf finding).
//!
//! The rewrite MUST stay byte-identical to the old kernel for every case —
//! disjoint/opaque, overlapping, partial opacity, and off-canvas placement —
//! and the all-background case must equal a solid fill of the background-encoded
//! constant. The old kernel is preserved verbatim as the **oracle**
//! ([`composite_reference`]); these tests pin production against it.
//!
//! A generous per-tick budget smoke (`tile_driven_meets_budget_9up_1080p`) makes
//! the per-tick budget a CI `#[test]`, not only a `cargo bench` (ADR-0022): the
//! tile-driven kernel must render a 9-up 1080p canvas well under one 25 fps tick.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::time::{Duration, Instant};

use mosaic_compositor::blend::LinearRgba;
use mosaic_compositor::pipeline::{
    canvas_linear_to_output_yuv, composite_reference, composite_with_threads, CanvasColor,
    Nv12Image, Tile,
};
use mosaic_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};
use proptest::prelude::*;

fn bt709_limited() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristic::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    }
}

fn pq_bt2020() -> ColorInfo {
    ColorInfo {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristic::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    }
}

/// Build a small NV12 image whose Y and chroma vary deterministically with a
/// per-image `seed`, so a wrong sample/stride is detectable and tiles differ
/// from one another. All dimensions are forced even (4:2:0).
fn varied_image(width: u32, height: u32, seed: u32, color: ColorInfo) -> Nv12Image {
    let w = usize::try_from(width).unwrap();
    let h = usize::try_from(height).unwrap();
    let mut y = vec![0_u8; w * h];
    let mut uv = vec![0_u8; w * h / 2];
    for row in 0..h {
        for col in 0..w {
            let v = (row.wrapping_mul(7) ^ col.wrapping_mul(5) ^ seed as usize) % 220;
            y[row * w + col] = 16 + u8::try_from(v).unwrap_or(0);
        }
    }
    for crow in 0..h / 2 {
        for cpair in 0..w / 2 {
            let idx = crow * w + cpair * 2;
            let cb = (crow.wrapping_mul(11) ^ cpair.wrapping_mul(3) ^ seed as usize) % 240;
            let cr = (crow.wrapping_mul(3) ^ cpair.wrapping_mul(13) ^ seed as usize) % 240;
            uv[idx] = u8::try_from(cb).unwrap_or(128);
            uv[idx + 1] = u8::try_from(cr).unwrap_or(128);
        }
    }
    Nv12Image::new(width, height, y, uv, color).unwrap()
}

/// Assert that the production kernel (both LUT and oracle transfer paths, both
/// serial and parallel) is byte-identical to the reference kernel for the given
/// canvas + tile stack.
fn assert_equivalent(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
) {
    for &use_lut in &[true, false] {
        let reference =
            composite_reference(canvas_w, canvas_h, canvas, background, tiles, use_lut).unwrap();
        for &n in &[1_usize, 2, 3, 8] {
            let prod = composite_with_threads(
                canvas_w, canvas_h, canvas, background, tiles, use_lut, n,
            )
            .unwrap();
            assert_eq!(
                prod.y_plane(),
                reference.y_plane(),
                "Y plane diverged from reference (use_lut={use_lut}, threads={n})"
            );
            assert_eq!(
                prod.uv_plane(),
                reference.uv_plane(),
                "UV plane diverged from reference (use_lut={use_lut}, threads={n})"
            );
            assert_eq!(
                prod.color(),
                reference.color(),
                "color tag diverged from reference (use_lut={use_lut}, threads={n})"
            );
        }
    }
}

#[test]
fn all_background_canvas_is_solid_background_constant() {
    // With no tiles, every pixel takes the background, encoded ONCE through the
    // back half. The whole frame must equal a solid fill of that constant —
    // bit-for-bit (this is exactly the precompute the rewrite relies on).
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.37, 0.81);
    let canvas_w = 64;
    let canvas_h = 36;

    let out = composite_with_threads(canvas_w, canvas_h, canvas, bg, &[], true, 4).unwrap();

    // Encode the background straight color once via the public back half.
    let straight = bg.premultiplied().unpremultiplied();
    let yuv = canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas).unwrap();
    let expected =
        Nv12Image::solid(canvas_w, canvas_h, yuv[0], yuv[1], yuv[2], canvas.output_tag()).unwrap();

    assert_eq!(out.y_plane(), expected.y_plane(), "Y plane != solid bg");
    assert_eq!(out.uv_plane(), expected.uv_plane(), "UV plane != solid bg");
    assert_eq!(out, expected, "all-background canvas != Nv12Image::solid(bg)");
}

#[test]
fn disjoint_opaque_grid_matches_reference() {
    // 9-up disjoint grid, all opaque — the common multiview case.
    let images: Vec<Nv12Image> = (0..9)
        .map(|i| varied_image(20, 12, i, bt709_limited()))
        .collect();
    let tiles: Vec<Tile<'_>> = images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let iu = u32::try_from(i).unwrap_or(0);
            Tile {
                image: img,
                dst_x: (iu % 3) * 20,
                dst_y: (iu / 3) * 12,
                opacity: 1.0,
            }
        })
        .collect();
    assert_equivalent(
        60,
        36,
        CanvasColor::default(),
        LinearRgba::opaque(0.05, 0.05, 0.05),
        &tiles,
    );
}

#[test]
fn overlapping_partial_opacity_matches_reference() {
    // Three overlapping tiles, all partially transparent, so the per-pixel
    // back-to-front fold order is load-bearing.
    let a = varied_image(40, 30, 1, bt709_limited());
    let b = varied_image(40, 30, 2, pq_bt2020());
    let c = varied_image(40, 30, 3, bt709_limited());
    let tiles = [
        Tile { image: &a, dst_x: 0, dst_y: 0, opacity: 0.7 },
        Tile { image: &b, dst_x: 10, dst_y: 8, opacity: 0.4 },
        Tile { image: &c, dst_x: 18, dst_y: 14, opacity: 0.55 },
    ];
    assert_equivalent(
        64,
        48,
        CanvasColor::default(),
        LinearRgba::opaque(0.1, 0.2, 0.3),
        &tiles,
    );
}

#[test]
fn off_canvas_and_clipped_tiles_match_reference() {
    // Tiles partially (and fully) off-canvas: top-left negative-ish via large
    // offsets near the edges, plus one entirely off-canvas (no-op).
    let big = varied_image(50, 50, 9, bt709_limited());
    let edge = varied_image(20, 20, 8, bt709_limited());
    let away = varied_image(10, 10, 7, bt709_limited());
    let tiles = [
        // Overhangs the right + bottom edges.
        Tile { image: &big, dst_x: 30, dst_y: 20, opacity: 1.0 },
        // Hugs the bottom-right corner, partly clipped.
        Tile { image: &edge, dst_x: 56, dst_y: 40, opacity: 0.8 },
        // Entirely off-canvas (dst beyond canvas): contributes nothing.
        Tile { image: &away, dst_x: 200, dst_y: 200, opacity: 1.0 },
    ];
    assert_equivalent(
        64,
        48,
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
        &tiles,
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The tile-driven production kernel is BIT-IDENTICAL to the reference
    /// kernel over random tile placements including overlap, opacity < 1, and
    /// off-canvas destinations, on both transfer paths and across thread counts.
    #[test]
    fn prop_tile_driven_bit_identical_to_reference(
        // Canvas is small + even; tiles vary in size, position (incl. off-canvas),
        // opacity, and color.
        seeds in proptest::collection::vec(
            (0_u32..1000, 0_u32..80, 0_u32..60, 4_u32..40, 4_u32..30, 0.0_f32..=1.0, any::<bool>()),
            0..=5,
        ),
        bg in (0.0_f32..=1.0, 0.0_f32..=1.0, 0.0_f32..=1.0, 0.0_f32..=1.0),
    ) {
        let canvas_w = 48_u32;
        let canvas_h = 32_u32;
        let canvas = CanvasColor::default();
        let background = LinearRgba { r: bg.0, g: bg.1, b: bg.2, a: bg.3 };

        // Materialize images (force even dims), then borrow into tiles.
        let images: Vec<Nv12Image> = seeds
            .iter()
            .map(|&(seed, _x, _y, w, h, _op, is_pq)| {
                let ew = (w & !1).max(2);
                let eh = (h & !1).max(2);
                let color = if is_pq { pq_bt2020() } else { bt709_limited() };
                varied_image(ew, eh, seed, color)
            })
            .collect();
        let tiles: Vec<Tile<'_>> = images
            .iter()
            .zip(seeds.iter())
            .map(|(img, &(_seed, x, y, _w, _h, op, _pq))| Tile {
                image: img,
                dst_x: x,
                dst_y: y,
                opacity: op,
            })
            .collect();

        for &use_lut in &[true, false] {
            let reference = composite_reference(
                canvas_w, canvas_h, canvas, background, &tiles, use_lut,
            ).unwrap();
            for &n in &[1_usize, 3, 8] {
                let prod = composite_with_threads(
                    canvas_w, canvas_h, canvas, background, &tiles, use_lut, n,
                ).unwrap();
                prop_assert_eq!(
                    prod.y_plane(),
                    reference.y_plane(),
                    "Y diverged (use_lut={}, threads={})", use_lut, n
                );
                prop_assert_eq!(
                    prod.uv_plane(),
                    reference.uv_plane(),
                    "UV diverged (use_lut={}, threads={})", use_lut, n
                );
            }
        }
    }
}

/// Build a solid / split / PQ rotation of `n` grid tiles covering 1080p, mirror
/// of the bench's `build_sources`/`build_tiles`, for the budget smoke.
fn nine_up_1080p() -> Vec<Nv12Image> {
    let cols = 3_u32;
    let rows = 3_u32;
    let tile_w = ((1920 / cols) & !1).max(2);
    let tile_h = ((1080 / rows) & !1).max(2);
    (0..9)
        .map(|i| match i % 3 {
            0 => Nv12Image::solid(tile_w, tile_h, 120, 100, 150, bt709_limited()).unwrap(),
            1 => varied_image(tile_w, tile_h, i, bt709_limited()),
            _ => varied_image(tile_w, tile_h, i, pq_bt2020()),
        })
        .collect()
}

#[test]
fn tile_driven_meets_budget_9up_1080p() {
    // CI per-tick budget gate as a #[test] (not only a bench). A debug build is
    // far slower than release, so the ceiling is generous (150 ms) — it guards
    // against the O(pixels × tiles) regression returning (the old kernel ran
    // ~18.6M coverage tests/frame here), not against absolute release timing.
    const CANVAS_W: u32 = 1920;
    const CANVAS_H: u32 = 1080;
    const CEILING: Duration = Duration::from_millis(150);

    let images = nine_up_1080p();
    let cols = 3_u32;
    let tile_w = ((1920 / cols) & !1).max(2);
    let tile_h = ((1080 / 3) & !1).max(2);
    let tiles: Vec<Tile<'_>> = images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let iu = u32::try_from(i).unwrap_or(0);
            Tile {
                image: img,
                dst_x: ((iu % cols) * tile_w) & !1,
                dst_y: ((iu / cols) * tile_h) & !1,
                opacity: 1.0,
            }
        })
        .collect();
    let canvas = CanvasColor::default();
    let bg = LinearRgba::opaque(0.02, 0.02, 0.02);

    // Warm-up, then median of 5.
    let warm = composite_with_threads(CANVAS_W, CANVAS_H, canvas, bg, &tiles, true, 4).unwrap();
    std::hint::black_box(&warm);
    let mut durations: Vec<Duration> = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        let out = composite_with_threads(CANVAS_W, CANVAS_H, canvas, bg, &tiles, true, 4).unwrap();
        durations.push(start.elapsed());
        std::hint::black_box(&out);
    }
    durations.sort_unstable();
    let median = durations[durations.len() / 2];
    assert!(
        median <= CEILING,
        "tile-driven composite 1080p×9 median {median:?} exceeds {CEILING:?} \
         (the O(pixels × tiles) regression may have returned)"
    );
}
