//! Integration tests for the overlay compositing SUB-PASS (Stage 2, ADR-0016
//! §4.1): blend overlay glyph quads + analytic primitives (filled/rounded
//! rects, lines/borders, meter bars) premultiplied-source-over the linear
//! canvas, with the CPU reference matching the GPU path.
//!
//! REAL pixel assertions in linear premultiplied space: a glyph quad, a meter
//! bar, and a safe-area rect land at known positions with correct coverage/
//! color; a tally border inks the expected edge pixels and leaves the interior
//! untouched; the GPU overlay WGSL `naga`-validates (feature `overlay+wgpu`).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]

use mosaic_compositor::blend::PremulRgba;
use mosaic_compositor::overlay::subpass::{
    blend_overlays, meter_bar, LinearCanvasBuffer, OverlayColor, OverlayDrawList, OverlayPrimitive,
    OverlayRect,
};
use mosaic_compositor::overlay::text::{FontFamily, TextEngine};

/// Opaque linear red / green / white overlay colors.
const RED: OverlayColor = OverlayColor::opaque(1.0, 0.0, 0.0);
const GREEN: OverlayColor = OverlayColor::opaque(0.0, 1.0, 0.0);
const WHITE_TEXT: [f32; 4] = [1.0, 1.0, 1.0, 1.0];

/// A pixel's straight-alpha linear RGBA, for assertions.
fn straight(canvas: &LinearCanvasBuffer, x: u32, y: u32) -> (f32, f32, f32, f32) {
    let p = canvas.pixel(x, y).expect("pixel in bounds");
    if p.a == 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (p.r / p.a, p.g / p.a, p.b / p.a, p.a)
}

#[test]
fn filled_rect_inks_its_pixels_and_leaves_the_rest_transparent() {
    let mut canvas = LinearCanvasBuffer::transparent(16, 16);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(4, 4, 6, 5),
        corner_radius: 0,
        color: GREEN,
    });

    blend_overlays(&mut canvas, &list);

    // Inside the rect: opaque green (premultiplied: rgb == color*alpha == green).
    let inside = canvas.pixel(5, 6).unwrap();
    assert_eq!(inside.a, 1.0, "interior pixel is opaque");
    assert_eq!((inside.r, inside.g, inside.b), (0.0, 1.0, 0.0));

    // Exactly the 6x5 box is inked; a pixel just outside is untouched.
    assert_eq!(canvas.pixel(10, 6).unwrap().a, 0.0, "right of the rect");
    assert_eq!(canvas.pixel(5, 9).unwrap().a, 0.0, "below the rect");
    assert_eq!(canvas.pixel(3, 6).unwrap().a, 0.0, "left of the rect");
    assert_eq!(canvas.pixel(5, 3).unwrap().a, 0.0, "above the rect");

    // Every covered pixel, and only those, is inked: count == area.
    let inked = (0..16)
        .flat_map(|y| (0..16).map(move |x| (x, y)))
        .filter(|&(x, y)| canvas.pixel(x, y).unwrap().a > 0.0)
        .count();
    assert_eq!(inked, 6 * 5, "exactly the rect area is covered");
}

#[test]
fn meter_bar_fills_left_to_right_by_fraction() {
    // A 10px-wide track at 60% fills the left 6 columns, leaves 4 empty.
    let mut canvas = LinearCanvasBuffer::transparent(16, 8);
    let track = OverlayRect::new(2, 2, 10, 4);
    let mut list = OverlayDrawList::new();
    list.push(meter_bar(track, 0.6, false, RED));

    blend_overlays(&mut canvas, &list);

    // Filled region: columns [2, 8) — opaque red.
    for x in 2..8 {
        let p = canvas.pixel(x, 3).unwrap();
        assert_eq!(p.a, 1.0, "filled column {x} is opaque");
        assert_eq!((p.r, p.g, p.b), (1.0, 0.0, 0.0), "filled column {x} is red");
    }
    // Unfilled region: columns [8, 12) — transparent.
    for x in 8..12 {
        assert_eq!(
            canvas.pixel(x, 3).unwrap().a,
            0.0,
            "unfilled column {x} stays transparent"
        );
    }
}

#[test]
fn safe_area_rect_blends_over_an_existing_background() {
    // Opaque mid-gray background; a 50%-alpha white safe-area fill over it must
    // lighten the covered region toward white by exactly the over operator.
    let bg = PremulRgba {
        r: 0.25,
        g: 0.25,
        b: 0.25,
        a: 1.0,
    };
    let mut canvas = LinearCanvasBuffer::filled(8, 8, bg);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(2, 2, 3, 3),
        corner_radius: 0,
        color: OverlayColor::new(1.0, 1.0, 1.0, 0.5),
    });

    blend_overlays(&mut canvas, &list);

    // over: out = src + dst*(1-a). src = (0.5,0.5,0.5,0.5) premul, dst gray 1.0.
    // out.rgb = 0.5 + 0.25*0.5 = 0.625 ; out.a = 0.5 + 1.0*0.5 = 1.0.
    let (r, g, b, a) = straight(&canvas, 3, 3);
    assert!((a - 1.0).abs() < 1e-6, "result stays opaque, got {a}");
    for (name, v) in [("r", r), ("g", g), ("b", b)] {
        assert!((v - 0.625).abs() < 1e-5, "{name} = {v}, expected 0.625");
    }
    // A pixel outside the fill keeps the untouched background.
    let (r0, _, _, a0) = straight(&canvas, 6, 6);
    assert_eq!((r0, a0), (0.25, 1.0), "uncovered background unchanged");
}

#[test]
fn tally_border_inks_only_the_edge_pixels() {
    // A 12x10 canvas with a 2px-thick tally border drawn as four line strokes;
    // the border rings the edge and the interior stays empty.
    let w = 12_u32;
    let h = 10_u32;
    let t = 2_u32;
    let mut canvas = LinearCanvasBuffer::transparent(w, h);
    let mut list = OverlayDrawList::new();
    // top, bottom, left, right strokes.
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(0, 0, w, t),
        color: RED,
    });
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(0, i32::try_from(h - t).unwrap(), w, t),
        color: RED,
    });
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(0, 0, t, h),
        color: RED,
    });
    list.push(OverlayPrimitive::Line {
        rect: OverlayRect::new(i32::try_from(w - t).unwrap(), 0, t, h),
        color: RED,
    });

    blend_overlays(&mut canvas, &list);

    // Edge pixels are inked red on each border.
    assert_eq!(canvas.pixel(0, 0).unwrap().a, 1.0, "top-left corner inked");
    assert_eq!(canvas.pixel(w - 1, 0).unwrap().a, 1.0, "top edge inked");
    assert_eq!(canvas.pixel(0, h - 1).unwrap().a, 1.0, "bottom-left inked");
    assert_eq!(canvas.pixel(w - 1, h - 1).unwrap().a, 1.0, "bottom-right");
    let (r, _, _, _) = straight(&canvas, 5, 0);
    assert_eq!(r, 1.0, "top border is red");

    // Just inside the 2px border ring, the interior is untouched.
    for y in t..(h - t) {
        for x in t..(w - t) {
            assert_eq!(
                canvas.pixel(x, y).unwrap().a,
                0.0,
                "interior pixel ({x},{y}) stays transparent"
            );
        }
    }
}

#[test]
fn glyph_quad_blends_real_swash_coverage_at_its_dest() {
    // The CPU reference blits the SAME swash coverage the GPU path uses (T7):
    // rasterize a digit, tint it green, blend at a known offset, and assert the
    // canvas inked exactly where the glyph has coverage, tinted by the color.
    let mut engine = TextEngine::new().expect("bundled fonts load");
    let run = engine
        .rasterize_run("7", FontFamily::Mono, 24.0, WHITE_TEXT)
        .expect("rasterize '7'");
    let glyph = &run.glyphs()[0];
    assert!(glyph.width > 0 && glyph.height > 0, "glyph has a real box");

    // Place the glyph box top-left at (10, 8) on a generous canvas.
    let (ox, oy) = (10_i32, 8_i32);
    let primitive = OverlayPrimitive::Glyph {
        dest_x: ox,
        dest_y: oy,
        width: glyph.width,
        height: glyph.height,
        coverage: glyph
            .premultiplied_rgba
            .chunks_exact(4)
            .map(|px| px[3])
            .collect(),
        color: GREEN,
    };
    let canvas_w = u32::try_from(ox).unwrap() + glyph.width + 16;
    let canvas_h = u32::try_from(oy).unwrap() + glyph.height + 16;
    let mut canvas = LinearCanvasBuffer::transparent(canvas_w, canvas_h);
    let mut list = OverlayDrawList::new();
    list.push(primitive);
    blend_overlays(&mut canvas, &list);

    // For every glyph pixel: canvas alpha matches the straight coverage exactly
    // (color.a == 1.0), and the inked color is green (premultiplied rgb == a).
    let mut inked = 0_usize;
    for row in 0..glyph.height {
        for col in 0..glyph.width {
            let cov_idx = usize::try_from(row).unwrap() * usize::try_from(glyph.width).unwrap()
                + usize::try_from(col).unwrap();
            let cov = glyph.premultiplied_rgba[cov_idx * 4 + 3];
            let cx = u32::try_from(ox).unwrap() + col;
            let cy = u32::try_from(oy).unwrap() + row;
            let p = canvas.pixel(cx, cy).unwrap();
            let expected_a = f32::from(cov) / 255.0;
            assert!(
                (p.a - expected_a).abs() < 1e-6,
                "glyph pixel ({col},{row}) alpha {} != coverage {expected_a}",
                p.a
            );
            if cov != 0 {
                inked += 1;
                // Green tint, premultiplied: r==0, g==alpha, b==0.
                assert!(p.r.abs() < 1e-6, "no red in a green glyph");
                assert!((p.g - p.a).abs() < 1e-6, "green channel == coverage");
                assert!(p.b.abs() < 1e-6, "no blue in a green glyph");
            }
        }
    }
    assert!(inked > 0, "the glyph actually inked some pixels");

    // The canvas region outside the glyph box is untouched.
    assert_eq!(
        canvas.pixel(canvas_w - 1, canvas_h - 1).unwrap().a,
        0.0,
        "far corner stays transparent"
    );
}

#[test]
fn batched_list_is_drawn_back_to_front() {
    // Two overlapping opaque rects: the LATER one (front) wins where they
    // overlap, proving the sub-pass honors draw order.
    let mut canvas = LinearCanvasBuffer::transparent(8, 8);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(1, 1, 4, 4),
        corner_radius: 0,
        color: RED,
    });
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(3, 3, 4, 4),
        corner_radius: 0,
        color: GREEN,
    });
    blend_overlays(&mut canvas, &list);

    // Overlap region (3,3): the front (green) rect wins.
    let p = canvas.pixel(3, 3).unwrap();
    assert_eq!(
        (p.r, p.g, p.b, p.a),
        (0.0, 1.0, 0.0, 1.0),
        "front wins overlap"
    );
    // Back-only region keeps red.
    let back = canvas.pixel(1, 1).unwrap();
    assert_eq!(
        (back.r, back.g, back.b),
        (1.0, 0.0, 0.0),
        "back-only is red"
    );
}

#[test]
fn rounded_rect_softens_corners_but_fills_the_core() {
    // A rounded rect leaves the extreme corner pixel less than fully covered
    // while the center is solid — the closed-form coverage the GPU SDF mirrors.
    let mut canvas = LinearCanvasBuffer::transparent(20, 20);
    let mut list = OverlayDrawList::new();
    list.push(OverlayPrimitive::FilledRect {
        rect: OverlayRect::new(2, 2, 16, 16),
        corner_radius: 5,
        color: GREEN,
    });
    blend_overlays(&mut canvas, &list);

    // Center is fully covered.
    assert_eq!(canvas.pixel(10, 10).unwrap().a, 1.0, "core is solid");
    // The extreme top-left corner pixel of the box is rounded away (alpha < 1).
    assert!(
        canvas.pixel(2, 2).unwrap().a < 1.0,
        "corner pixel is rounded (alpha {})",
        canvas.pixel(2, 2).unwrap().a
    );
}
