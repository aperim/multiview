//! The overlay compositing **sub-pass**: blend overlay glyph quads and analytic
//! vector primitives premultiplied-source-over into the `Rgba16Float` **linear**
//! canvas, between the composite pass and the NV12 encode pass (ADR-0016 §4.1,
//! overlay-rendering.md §4.1, invariants #5 + #8).
//!
//! This module is the **CPU reference** half plus the shared, GPU-free model the
//! GPU compute path consumes ([`crate::overlay::gpu_subpass`], feature
//! `overlay` + `wgpu`). Both blend the **same** primitives into the same linear
//! premultiplied accumulator with [`crate::blend::over`], so GPU ≈ CPU
//! (overlay-rendering.md §4.4 / T7).
//!
//! # The model
//!
//! Every overlay reduces to one of a small set of premultiplied-**linear**
//! primitives ([`OverlayPrimitive`]):
//!
//! - [`OverlayPrimitive::Glyph`] — a swash coverage bitmap (straight 8-bit
//!   coverage in its alpha channel; see [`crate::overlay::text`]) tinted with a
//!   **linear** [`OverlayColor`] and blended `over` at a dest pixel. Text.
//! - [`OverlayPrimitive::FilledRect`] — a solid (optionally rounded) rectangle:
//!   alert-card chrome, tally-border fills, bar-fill meters, IDENTIFY flash.
//! - [`OverlayPrimitive::Line`] — an axis-aligned hairline/border of a given
//!   thickness: safe-area boxes, center-cross arms, tally-border edges.
//!
//! All colors are **linear**, premultiplied at blend time (swash coverage is
//! straight; ADR-R008 / invariant #8). The CPU canvas is a dense buffer of
//! premultiplied-linear `[f32; 4]` pixels ([`LinearCanvasBuffer`]); the GPU path
//! runs the identical math over the `Rgba16Float` storage texture.
//!
//! Batching (T5): [`OverlayDrawList`] holds **all** the frame's primitives in
//! one back-to-front list; the sub-pass walks it once, so the cost is constant
//! in the tile count.

use crate::blend::{over, LinearRgba, PremulRgba};
use crate::error::Result;
use crate::overlay::text::{RasterizedGlyph, RasterizedRun};
use crate::pipeline::{
    canvas_linear_to_output_yuv, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image,
};

/// A straight (non-premultiplied) **linear-light** RGBA overlay color in the
/// canvas working space, channels in `0.0..=1.0`.
///
/// Overlay colors authored in sRGB must already be linearized into the canvas
/// gamut by the caller (overlay-rendering.md §4.1); this type *is* that linear
/// value. It is premultiplied at blend time (swash coverage is straight).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayColor {
    /// Linear red, `0.0..=1.0`.
    pub r: f32,
    /// Linear green, `0.0..=1.0`.
    pub g: f32,
    /// Linear blue, `0.0..=1.0`.
    pub b: f32,
    /// Straight alpha, `0.0..=1.0`.
    pub a: f32,
}

impl OverlayColor {
    /// An opaque linear overlay color.
    #[must_use]
    pub const fn opaque(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    /// A linear overlay color with an explicit straight alpha.
    #[must_use]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// The straight-alpha [`LinearRgba`] this color denotes.
    #[must_use]
    pub const fn linear_rgba(self) -> LinearRgba {
        LinearRgba {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a,
        }
    }
}

/// An axis-aligned pixel rectangle (top-left origin), in integer pixels.
///
/// Sub-pixel placement is a GPU-fast-path nicety (ADR-R008); the CPU oracle
/// rasterizes on the integer grid so its result is exactly assertable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayRect {
    /// Left edge, pixels.
    pub x: i32,
    /// Top edge, pixels.
    pub y: i32,
    /// Width, pixels.
    pub width: u32,
    /// Height, pixels.
    pub height: u32,
}

impl OverlayRect {
    /// Construct a rectangle.
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// One overlay primitive in premultiplied-linear space, ready to blend `over`
/// the canvas. The CPU reference and the GPU sub-pass both consume these.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum OverlayPrimitive {
    /// A shaped glyph: a swash coverage bitmap tinted by a linear color and
    /// blended at `dest`. `coverage` is the **premultiplied-RGBA8** bitmap from
    /// [`RasterizedGlyph`]; only its alpha channel (straight coverage) is used,
    /// the linear `color` supplies the tint (so HDR programs keep correct
    /// overlay color, overlay-rendering.md §4.1).
    Glyph {
        /// Top-left destination of the coverage box, in canvas pixels.
        dest_x: i32,
        /// Top-left destination of the coverage box, in canvas pixels.
        dest_y: i32,
        /// Coverage box width, pixels.
        width: u32,
        /// Coverage box height, pixels.
        height: u32,
        /// Per-pixel straight coverage, row-major (`width * height` bytes). This
        /// is the alpha channel of the swash premultiplied-RGBA8 bitmap.
        coverage: Vec<u8>,
        /// Linear tint applied to the coverage.
        color: OverlayColor,
    },
    /// A solid (optionally rounded) filled rectangle. `corner_radius` of `0`
    /// is a plain rectangle (alert-card chrome, bar-fill meters, tally fills,
    /// IDENTIFY flash).
    FilledRect {
        /// The rectangle, integer pixels.
        rect: OverlayRect,
        /// Corner radius in pixels (`0` = square corners). Rounded corners use a
        /// closed-form coverage falloff identical on CPU and GPU.
        corner_radius: u32,
        /// Linear fill color.
        color: OverlayColor,
    },
    /// An axis-aligned line / border stroke of a given thickness, drawn as a
    /// rectangle (safe-area edges, center-cross arms, tally borders).
    Line {
        /// The stroke rectangle, integer pixels (thickness is its short side).
        rect: OverlayRect,
        /// Linear stroke color.
        color: OverlayColor,
    },
}

impl OverlayPrimitive {
    /// Build a glyph primitive from a stage-1 [`RasterizedGlyph`], tinting its
    /// straight coverage with a linear overlay `color`.
    ///
    /// The glyph's own premultiplied RGB is discarded; only its alpha (coverage)
    /// is kept, so the same swash bitmap renders in any linear color without
    /// re-rasterizing (overlay-rendering.md §4.1 / T2).
    #[must_use]
    pub fn glyph(glyph: &RasterizedGlyph, color: OverlayColor) -> Self {
        let coverage = glyph
            .premultiplied_rgba
            .chunks_exact(4)
            .filter_map(|px| px.get(3).copied())
            .collect();
        Self::Glyph {
            dest_x: glyph.dest_x,
            dest_y: glyph.dest_y,
            width: glyph.width,
            height: glyph.height,
            coverage,
            color,
        }
    }
}

/// A horizontal bar-fill meter (PPM/VU/peak), the canonical analytic primitive
/// (overlay-rendering.md §4.2: "meters are geometry, not pictures").
///
/// `fill` in `0.0..=1.0` fills the track left→right (or, when `vertical`,
/// bottom→up) into one [`OverlayPrimitive::FilledRect`]. No bitmap, no upload.
#[must_use]
pub fn meter_bar(
    track: OverlayRect,
    fill: f32,
    vertical: bool,
    color: OverlayColor,
) -> OverlayPrimitive {
    let frac = fill.clamp(0.0, 1.0);
    if vertical {
        let filled = scale_u32(track.height, frac);
        let top = track
            .y
            .saturating_add(i32_from_u32(track.height.saturating_sub(filled)));
        OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(track.x, top, track.width, filled),
            corner_radius: 0,
            color,
        }
    } else {
        let filled = scale_u32(track.width, frac);
        OverlayPrimitive::FilledRect {
            rect: OverlayRect::new(track.x, track.y, filled, track.height),
            corner_radius: 0,
            color,
        }
    }
}

/// The full, ordered overlay primitive list for one frame, back-to-front (index
/// `0` is drawn first / furthest back). One gather per frame (T5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OverlayDrawList {
    /// The primitives, in draw (back-to-front) order.
    pub primitives: Vec<OverlayPrimitive>,
}

impl OverlayDrawList {
    /// An empty draw list.
    #[must_use]
    pub fn new() -> Self {
        Self {
            primitives: Vec::new(),
        }
    }

    /// Append one primitive (back-to-front).
    pub fn push(&mut self, primitive: OverlayPrimitive) {
        self.primitives.push(primitive);
    }

    /// Append every glyph of a rasterized text run, tinted with `color`.
    pub fn push_run(&mut self, run: &RasterizedRun, color: OverlayColor) {
        for glyph in run.glyphs() {
            self.primitives.push(OverlayPrimitive::glyph(glyph, color));
        }
    }

    /// Number of primitives queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.primitives.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

/// A dense CPU canvas of **premultiplied-linear** RGBA `[f32; 4]` pixels.
///
/// This is the CPU mirror of the GPU `Rgba16Float` linear canvas: the composite
/// pass would seed it (premultiplied), the overlay sub-pass blends `over` it,
/// and the encode pass reads it back. The overlay sub-pass only ever *reads then
/// writes* existing pixels — it never reallocates (T3 zero per-frame alloc on
/// the GPU; the CPU oracle is a test/reference path).
#[derive(Debug, Clone, PartialEq)]
pub struct LinearCanvasBuffer {
    width: u32,
    height: u32,
    /// Row-major premultiplied-linear RGBA, `width * height` pixels.
    pixels: Vec<[f32; 4]>,
}

impl LinearCanvasBuffer {
    /// A fully transparent canvas of `width × height` premultiplied pixels.
    #[must_use]
    pub fn transparent(width: u32, height: u32) -> Self {
        let count = usize::try_from(width)
            .ok()
            .and_then(|w| usize::try_from(height).ok().map(|h| w.saturating_mul(h)))
            .unwrap_or(0);
        Self {
            width,
            height,
            pixels: vec![[0.0; 4]; count],
        }
    }

    /// A canvas filled with one premultiplied-linear background color.
    #[must_use]
    pub fn filled(width: u32, height: u32, background: PremulRgba) -> Self {
        let mut canvas = Self::transparent(width, height);
        let bg = [background.r, background.g, background.b, background.a];
        for px in &mut canvas.pixels {
            *px = bg;
        }
        canvas
    }

    /// Canvas width, pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height, pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The backing pixel buffer's heap **capacity**, in pixels — a stable
    /// fingerprint of the allocation that holds the canvas.
    ///
    /// The overlay sub-pass blends **in place** (it only ever reads-then-writes
    /// existing pixels; see [`blend_overlays`]), never
    /// pushing or growing, so a steady-state frame must **never** reallocate this
    /// buffer (T3: zero per-frame heap allocation, ADR-0016). A reallocation would
    /// change the `Vec`'s capacity; an efficiency benchmark asserts the capacity is
    /// unchanged across a blend. (`Vec` never shrinks capacity on its own, so an
    /// unchanged capacity across the write-only blend means no allocation churn on
    /// the output buffer.)
    #[must_use]
    pub fn buffer_capacity(&self) -> usize {
        self.pixels.capacity()
    }

    /// The premultiplied-linear pixel at `(x, y)`, or `None` if out of bounds.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<PremulRgba> {
        let idx = self.index(x, y)?;
        self.pixels.get(idx).map(|p| PremulRgba {
            r: p[0],
            g: p[1],
            b: p[2],
            a: p[3],
        })
    }

    /// Linear index of `(x, y)` within bounds.
    fn index(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let w = usize::try_from(self.width).ok()?;
        let xi = usize::try_from(x).ok()?;
        let yi = usize::try_from(y).ok()?;
        yi.checked_mul(w)?.checked_add(xi)
    }

    /// Blend a premultiplied-linear `src` `over` the pixel at `(x, y)` (clipped).
    fn blend_at(&mut self, x: u32, y: u32, src: PremulRgba) {
        let Some(idx) = self.index(x, y) else {
            return;
        };
        let Some(slot) = self.pixels.get_mut(idx) else {
            return;
        };
        let dst = PremulRgba {
            r: slot[0],
            g: slot[1],
            b: slot[2],
            a: slot[3],
        };
        let out = over(src, dst);
        *slot = [out.r, out.g, out.b, out.a];
    }
}

/// Blend every primitive of `list` premultiplied-source-over the linear
/// `canvas`, in back-to-front order — the **CPU reference** overlay sub-pass.
///
/// This is the oracle the GPU compute path
/// ([`crate::overlay::gpu_subpass`]) is validated against: identical primitives,
/// identical [`over`] math, same premultiplied-linear accumulator.
pub fn blend_overlays(canvas: &mut LinearCanvasBuffer, list: &OverlayDrawList) {
    for primitive in &list.primitives {
        blend_primitive(canvas, primitive);
    }
}

/// Blend one primitive into the canvas.
fn blend_primitive(canvas: &mut LinearCanvasBuffer, primitive: &OverlayPrimitive) {
    match primitive {
        OverlayPrimitive::Glyph {
            dest_x,
            dest_y,
            width,
            height,
            coverage,
            color,
        } => blend_glyph(canvas, *dest_x, *dest_y, *width, *height, coverage, *color),
        OverlayPrimitive::FilledRect {
            rect,
            corner_radius,
            color,
        } => blend_filled_rect(canvas, *rect, *corner_radius, *color),
        OverlayPrimitive::Line { rect, color } => {
            // A line/border is a filled rectangle with square corners.
            blend_filled_rect(canvas, *rect, 0, *color);
        }
    }
}

/// Blend a glyph coverage bitmap tinted by `color` at `(dest_x, dest_y)`.
fn blend_glyph(
    canvas: &mut LinearCanvasBuffer,
    dest_x: i32,
    dest_y: i32,
    width: u32,
    height: u32,
    coverage: &[u8],
    color: OverlayColor,
) {
    for row in 0..height {
        for col in 0..width {
            let Some(idx) = pixel_index(width, col, row) else {
                continue;
            };
            let Some(&cov) = coverage.get(idx) else {
                continue;
            };
            if cov == 0 {
                continue;
            }
            let Some((cx, cy)) = canvas_xy(dest_x, dest_y, col, row) else {
                continue;
            };
            // Straight coverage scales the straight alpha; premultiply once.
            let alpha = color.a * unit_from_u8(cov);
            let src = LinearRgba {
                r: color.r,
                g: color.g,
                b: color.b,
                a: alpha,
            }
            .premultiplied();
            canvas.blend_at(cx, cy, src);
        }
    }
}

/// Blend a filled (optionally rounded) rectangle of `color` into the canvas.
///
/// A `corner_radius` of `0` is a sharp rectangle. Rounded corners use a
/// closed-form quarter-disc coverage: a pixel center inside the inset core or
/// within `radius` of a corner center is fully covered (coverage `1.0`); the
/// boundary pixel ring gets a linear `radius - distance` antialias falloff so
/// the CPU oracle and the GPU SDF agree on edge softness.
fn blend_filled_rect(
    canvas: &mut LinearCanvasBuffer,
    rect: OverlayRect,
    corner_radius: u32,
    color: OverlayColor,
) {
    if rect.width == 0 || rect.height == 0 || color.a <= 0.0 {
        return;
    }
    let radius = clamp_radius(corner_radius, rect.width, rect.height);
    for row in 0..rect.height {
        for col in 0..rect.width {
            let coverage = rect_coverage(rect.width, rect.height, col, row, radius);
            if coverage <= 0.0 {
                continue;
            }
            let Some((cx, cy)) = canvas_xy(rect.x, rect.y, col, row) else {
                continue;
            };
            let src = LinearRgba {
                r: color.r,
                g: color.g,
                b: color.b,
                a: color.a * coverage,
            }
            .premultiplied();
            canvas.blend_at(cx, cy, src);
        }
    }
}

/// Closed-form coverage of a rounded-rect at local pixel `(col, row)` with a
/// corner `radius` (`0` ⇒ a sharp rectangle, every pixel `1.0`). Returns
/// `0.0..=1.0`. The same math runs in the GPU shader.
fn rect_coverage(width: u32, height: u32, col: u32, row: u32, radius: u32) -> f32 {
    if radius == 0 {
        return 1.0;
    }
    let r = unit_dim(radius);
    // Pixel center in local rect coordinates.
    let px = unit_dim(col) + 0.5;
    let py = unit_dim(row) + 0.5;
    let w = unit_dim(width);
    let h = unit_dim(height);
    // Distance from the pixel center into the nearest rounded corner region.
    // dx/dy measure how far the center is *inside* each corner disc's bounding
    // axis; only when both are positive are we in a corner quadrant.
    let dx = (r - px).max(px - (w - r)).max(0.0);
    let dy = (r - py).max(py - (h - r)).max(0.0);
    if dx <= 0.0 || dy <= 0.0 {
        return 1.0;
    }
    let dist = (dx * dx + dy * dy).sqrt();
    // Linear 1px antialias falloff across the corner arc.
    (r - dist + 0.5).clamp(0.0, 1.0)
}

/// Clamp a requested corner radius to at most half the shorter side.
fn clamp_radius(radius: u32, width: u32, height: u32) -> u32 {
    let half = width.min(height) / 2;
    radius.min(half)
}

/// Row-major index of local `(col, row)` in a `width`-wide bitmap.
fn pixel_index(width: u32, col: u32, row: u32) -> Option<usize> {
    let w = usize::try_from(width).ok()?;
    let c = usize::try_from(col).ok()?;
    let r = usize::try_from(row).ok()?;
    r.checked_mul(w)?.checked_add(c)
}

/// Translate a local `(col, row)` offset from a signed dest origin to a
/// non-negative canvas `(x, y)`, or `None` if it lands left/above the canvas.
fn canvas_xy(dest_x: i32, dest_y: i32, col: u32, row: u32) -> Option<(u32, u32)> {
    let cx = dest_x.checked_add(i32_from_u32(col))?;
    let cy = dest_y.checked_add(i32_from_u32(row))?;
    let x = u32::try_from(cx).ok()?;
    let y = u32::try_from(cy).ok()?;
    Some((x, y))
}

/// `u8` coverage `0..=255` as a unit float `0.0..=1.0`.
fn unit_from_u8(v: u8) -> f32 {
    f32::from(v) / 255.0
}

/// Exact small-`u32` to `f32` (overlay sizes are well under `2^24`), no `as`.
fn unit_dim(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Saturating `u32 -> i32` (overlay coordinates are small), no `as`.
fn i32_from_u32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Scale a `u32` length by a `0.0..=1.0` fraction, rounded, without an `as`
/// cast: binary-search the integer whose unit image is nearest `len * frac`.
fn scale_u32(len: u32, frac: f32) -> u32 {
    let target = (unit_dim(len) * frac.clamp(0.0, 1.0)).round();
    let (mut lo, mut hi) = (0_u32, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if unit_dim(mid) < target {
            lo = mid.saturating_add(1);
        } else {
            hi = mid;
        }
    }
    lo
}

/// Blend an [`OverlayDrawList`] **into an existing NV12 canvas**, running the
/// overlay sub-pass through the fixed color pipeline (invariants #5 + #8).
///
/// This is the CPU-reference wire used by the engine/CLI to *bake* the resolved
/// overlays into the composited program before encode (the GPU path does this in
/// the `Rgba16Float` storage texture, [`crate::overlay::gpu_subpass`]). For each
/// pixel it: decodes the NV12 sample to linear canvas-gamut RGB (the front half
/// of the pipeline), seeds the linear premultiplied accumulator, blends every
/// primitive `over` it ([`blend_overlays`] math), then re-encodes the result to
/// NV12 (the back half). Pixels no primitive touches are decoded+re-encoded
/// identically, so an empty list is a no-op modulo color round-trip.
///
/// It runs **off the hot path** (on collected output frames), never blocks, and
/// never reallocates the planes. `canvas` describes the NV12 image's color so
/// the round-trip uses the canvas's own primaries/transfer/matrix/range.
///
/// # Errors
///
/// Returns the compositor [`crate::error::Error`] if the image color is
/// unresolved/unsupported (the same guard the composite pass applies).
pub fn apply_overlays_to_nv12(
    image: &Nv12Image,
    list: &OverlayDrawList,
    canvas: CanvasColor,
) -> Result<Nv12Image> {
    let width = image.width();
    let height = image.height();
    // Seed a dense linear premultiplied buffer from the NV12 image (decode).
    let mut buffer = LinearCanvasBuffer::transparent(width, height);
    let color = image.color();
    for y in 0..height {
        for x in 0..width {
            let Some((y8, cb8, cr8)) = image.sample(x, y) else {
                continue;
            };
            let lin = tile_yuv_to_canvas_linear(y8, cb8, cr8, color, canvas)?;
            // The program is opaque: alpha 1.0, premultiplied trivially.
            if let Some(idx) = buffer.index(x, y) {
                if let Some(slot) = buffer.pixels.get_mut(idx) {
                    *slot = [lin[0], lin[1], lin[2], 1.0];
                }
            }
        }
    }

    // Blend the overlay primitives over the seeded canvas (the shared math).
    blend_overlays(&mut buffer, list);

    // Re-encode the linear premultiplied buffer back to NV12 (the back half).
    let (w, h) = (
        usize::try_from(width).unwrap_or(0),
        usize::try_from(height).unwrap_or(0),
    );
    let mut y_plane = vec![0_u8; w.saturating_mul(h)];
    let mut uv_plane = vec![0_u8; w.saturating_mul(h) / 2];
    for y in 0..height {
        for x in 0..width {
            let premul = buffer.pixel(x, y).unwrap_or(PremulRgba::TRANSPARENT);
            let straight = premul.unpremultiplied();
            let out = canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?;
            write_nv12_pixel(&mut y_plane, &mut uv_plane, w, x, y, out);
        }
    }
    Nv12Image::new(width, height, y_plane, uv_plane, canvas.output_tag())
}

/// Write one output pixel's `(y, cb, cr)` into NV12 planes (chroma per pixel,
/// last-writer-wins in each 2×2 block — the reference's nearest-neighbour
/// model, matching [`crate::pipeline::composite`]).
fn write_nv12_pixel(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    w: usize,
    px: u32,
    py: u32,
    yuv: [u8; 3],
) {
    let (Ok(xi), Ok(yi)) = (usize::try_from(px), usize::try_from(py)) else {
        return;
    };
    if let Some(slot) = y_plane.get_mut(yi.saturating_mul(w).saturating_add(xi)) {
        *slot = yuv[0];
    }
    let cx = xi / 2;
    let cy = yi / 2;
    let uv_index = cy.saturating_mul(w).saturating_add(cx.saturating_mul(2));
    if let Some(slot) = uv_plane.get_mut(uv_index) {
        *slot = yuv[1];
    }
    if let Some(slot) = uv_plane.get_mut(uv_index.saturating_add(1)) {
        *slot = yuv[2];
    }
}
