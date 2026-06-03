//! The fixed-order color pipeline (invariant #8) and a small CPU reference
//! compositor over NV12.
//!
//! This module wires the pure color-math modules into the **one canonical
//! order** (color-management.md §2), which must never be reordered:
//!
//! detect 4 axes -> range expand -> YUV->RGB matrix -> linearize (EOTF) ->
//! primaries convert (in linear) -> scale + premultiplied-alpha blend (in
//! linear) -> OETF -> RGB->YUV + range compress -> tag output.
//!
//! The compositor here is a **CPU reference** (golden-frame, bit-exact on CPU);
//! the GPU/wgpu path is out of scope for this module and feature-gated off by
//! default. The canvas is fixed SDR BT.709 limited (ADR-C001); the working
//! blend space is linear BT.709-primaries RGB.

use mosaic_core::color::{
    ColorInfo, ColorPrimaries, ColorRange, MatrixCoefficients, TransferCharacteristic,
};

use crate::blend::{over, LinearRgba, PremulRgba};
use crate::error::{Error, Result};
use crate::transfer_lut::LutSet;
use crate::{matrix, primaries, range, transfer};

/// The fixed canvas color description (ADR-C001).
///
/// Default is SDR BT.709 limited: BT.709 primaries, BT.709/BT.1886 transfer,
/// BT.709-NCL matrix, limited range. The working blend buffer is linear in the
/// canvas **primaries**; the canvas transfer is applied (OETF) only on encode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasColor {
    /// Canvas primaries (gamut) — the linear blend space.
    pub primaries: ColorPrimaries,
    /// Canvas transfer (applied on the encode-side OETF).
    pub transfer: TransferCharacteristic,
    /// Canvas YUV<->RGB matrix (applied on the encode-side RGB->YUV).
    pub matrix: MatrixCoefficients,
    /// Canvas quantization range (applied on the encode-side compression).
    pub range: ColorRange,
}

impl Default for CanvasColor {
    /// The default SDR BT.709 limited canvas (ADR-C001).
    fn default() -> Self {
        Self {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristic::Bt709,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
        }
    }
}

impl CanvasColor {
    /// The [`ColorInfo`] this canvas tags its output with (the "tag output"
    /// step). Tagging only labels; the pixels were produced to match.
    #[must_use]
    pub const fn output_tag(self) -> ColorInfo {
        ColorInfo {
            primaries: self.primaries,
            transfer: self.transfer,
            matrix: self.matrix,
            range: self.range,
        }
    }
}

/// Convert one tile YUV sample (8-bit code values) through the **front half**
/// of the fixed pipeline into linear RGB in the **canvas gamut**, ready to
/// blend.
///
/// Order, exactly: range-expand -> YUV->RGB matrix (gamma R'G'B') ->
/// linearize (tile EOTF) -> primaries convert into the canvas gamut (linear).
///
/// `tile_color` must be fully resolved (no `Unspecified` axis); run
/// [`ColorInfo::resolve_defaults`] first.
///
/// # Errors
///
/// Returns [`Error::UnresolvedColor`] if any axis is unspecified, or the
/// `Unsupported*` variants when an axis has no CPU-reference implementation.
pub fn tile_yuv_to_canvas_linear(
    y8: u8,
    cb8: u8,
    cr8: u8,
    tile_color: ColorInfo,
    canvas: CanvasColor,
) -> Result<[f32; 3]> {
    // detect/resolve guard: the kernel never sees Unspecified.
    let tile_range = range::require_resolved(tile_color.range)?;
    if tile_color.transfer == TransferCharacteristic::Unspecified {
        return Err(Error::UnresolvedColor("transfer"));
    }
    if tile_color.matrix == MatrixCoefficients::Unspecified {
        return Err(Error::UnresolvedColor("matrix"));
    }
    if tile_color.primaries == ColorPrimaries::Unspecified {
        return Err(Error::UnresolvedColor("primaries"));
    }

    // 1. range expand (code-value space).
    let y = range::expand_luma(y8, tile_range);
    let cb = range::expand_chroma(cb8, tile_range);
    let cr = range::expand_chroma(cr8, tile_range);

    // 2. YUV' -> R'G'B' (gamma-encoded).
    let rgb_gamma = matrix::yuv_to_rgb(y, cb, cr, tile_color.matrix)?;

    // 3. linearize via the tile's own EOTF.
    let lin = [
        transfer::eotf(rgb_gamma[0], tile_color.transfer)?,
        transfer::eotf(rgb_gamma[1], tile_color.transfer)?,
        transfer::eotf(rgb_gamma[2], tile_color.transfer)?,
    ];

    // 4. primaries convert into the canvas gamut (linear light).
    let conv = primaries::convert_matrix(tile_color.primaries, canvas.primaries)?;
    Ok(primaries::apply(conv, lin))
}

/// Convert a linear RGB triple in the **canvas gamut** through the **back half**
/// of the fixed pipeline into output YUV 8-bit code values.
///
/// Order, exactly: canvas OETF -> RGB->YUV (canvas matrix) -> range compress.
///
/// # Errors
///
/// Returns an `Unsupported*` variant if a canvas axis has no CPU-reference
/// implementation.
pub fn canvas_linear_to_output_yuv(lin: [f32; 3], canvas: CanvasColor) -> Result<[u8; 3]> {
    // 6. canvas OETF (linear -> gamma code values).
    let gamma = [
        transfer::oetf(lin[0], canvas.transfer)?,
        transfer::oetf(lin[1], canvas.transfer)?,
        transfer::oetf(lin[2], canvas.transfer)?,
    ];
    // 7. RGB -> YUV with the canvas matrix.
    let yuv = matrix::rgb_to_yuv(gamma[0], gamma[1], gamma[2], canvas.matrix)?;
    // 8. range compress to 8-bit code values.
    Ok([
        range::compress_luma(yuv[0], canvas.range),
        range::compress_chroma(yuv[1], canvas.range),
        range::compress_chroma(yuv[2], canvas.range),
    ])
}

/// A tiny owned NV12 image: a `width x height` Y plane followed by a
/// `width x (height/2)` interleaved Cb/Cr plane (4:2:0 semi-planar).
///
/// This is the CPU-reference pixel container — the GPU path uses native
/// surfaces and never materializes one of these. Width and height are required
/// to be even (4:2:0 chroma subsampling).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Image {
    width: u32,
    height: u32,
    y_plane: Vec<u8>,
    uv_plane: Vec<u8>,
    /// The resolved color of these samples.
    color: ColorInfo,
}

impl Nv12Image {
    /// Build an NV12 image from explicit planes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Geometry`] if `width`/`height` are not both even and
    /// positive, or if a plane length does not match the geometry
    /// (`y = width*height`, `uv = width*height/2`).
    pub fn new(
        width: u32,
        height: u32,
        y_plane: Vec<u8>,
        uv_plane: Vec<u8>,
        color: ColorInfo,
    ) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::Geometry(format!(
                "NV12 dimensions must be positive and even (got {width}x{height})"
            )));
        }
        let pixels = usize::try_from(width)
            .ok()
            .zip(usize::try_from(height).ok())
            .ok_or_else(|| Error::Geometry("dimensions overflow usize".to_owned()))?;
        let y_len = pixels.0 * pixels.1;
        let uv_len = y_len / 2;
        if y_plane.len() != y_len {
            return Err(Error::Geometry(format!(
                "Y plane length {} != expected {y_len}",
                y_plane.len()
            )));
        }
        if uv_plane.len() != uv_len {
            return Err(Error::Geometry(format!(
                "UV plane length {} != expected {uv_len}",
                uv_plane.len()
            )));
        }
        Ok(Self {
            width,
            height,
            y_plane,
            uv_plane,
            color,
        })
    }

    /// A solid-color NV12 image filled with the given 8-bit `(y, cb, cr)` code
    /// values; useful for tests and placeholder cards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Geometry`] if `width`/`height` are not both even and
    /// positive.
    pub fn solid(width: u32, height: u32, y: u8, cb: u8, cr: u8, color: ColorInfo) -> Result<Self> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::Geometry(format!(
                "NV12 dimensions must be positive and even (got {width}x{height})"
            )));
        }
        let (w, h) = (
            usize::try_from(width).map_err(|_| Error::Geometry("width overflow".to_owned()))?,
            usize::try_from(height).map_err(|_| Error::Geometry("height overflow".to_owned()))?,
        );
        let y_plane = vec![y; w * h];
        let mut uv_plane = vec![0_u8; w * h / 2];
        for pair in uv_plane.chunks_exact_mut(2) {
            if let [u, v] = pair {
                *u = cb;
                *v = cr;
            }
        }
        Ok(Self {
            width,
            height,
            y_plane,
            uv_plane,
            color,
        })
    }

    /// Image width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Image height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The resolved color of these samples.
    #[must_use]
    pub const fn color(&self) -> ColorInfo {
        self.color
    }

    /// Borrow the Y plane.
    #[must_use]
    pub fn y_plane(&self) -> &[u8] {
        &self.y_plane
    }

    /// Borrow the interleaved Cb/Cr plane.
    #[must_use]
    pub fn uv_plane(&self) -> &[u8] {
        &self.uv_plane
    }

    /// Sample the `(y, cb, cr)` 8-bit code values at integer pixel `(x, y)`
    /// (nearest, no chroma interpolation — the reference uses simple chroma
    /// replication; correct chroma siting is a GPU concern out of scope here).
    ///
    /// Returns [`None`] if `(x, py)` is outside the image.
    #[must_use]
    pub fn sample(&self, x: u32, py: u32) -> Option<(u8, u8, u8)> {
        if x >= self.width || py >= self.height {
            return None;
        }
        let w = usize::try_from(self.width).ok()?;
        let xi = usize::try_from(x).ok()?;
        let yi = usize::try_from(py).ok()?;
        let y = *self.y_plane.get(yi * w + xi)?;
        // Chroma is subsampled 2x2; the interleaved row stride is `w` bytes
        // (w/2 chroma pairs). Locate the chroma pair for this pixel.
        let cx = xi / 2;
        let cy = yi / 2;
        let uv_index = (cy * w) + (cx * 2);
        let cb = *self.uv_plane.get(uv_index)?;
        let cr = *self.uv_plane.get(uv_index + 1)?;
        Some((y, cb, cr))
    }
}

/// One tile in the reference composite: a source image, where it lands on the
/// canvas, and a uniform opacity.
#[derive(Debug, Clone)]
pub struct Tile<'a> {
    /// The source NV12 image (its own resolved color).
    pub image: &'a Nv12Image,
    /// Destination x (pixels) of the tile's top-left on the canvas.
    pub dst_x: u32,
    /// Destination y (pixels) of the tile's top-left on the canvas.
    pub dst_y: u32,
    /// Uniform tile opacity in `[0, 1]` (straight alpha).
    pub opacity: f32,
}

/// Composite a back-to-front stack of [`Tile`]s onto a `canvas_w x canvas_h`
/// output, running the full fixed-order pipeline per pixel, and return the
/// tagged output NV12 image.
///
/// Pixels not covered by any tile take `background` (a linear canvas-gamut
/// color). Tiles are drawn in slice order (first = bottom); each tile is placed
/// 1:1 (no scaling — scaling is exercised by the math but a resampler is out of
/// scope for this reference) and clipped to the canvas. Per pixel the front
/// half ([`tile_yuv_to_canvas_linear`]) runs for each covering tile, the
/// premultiplied-alpha [`over`] operator folds them in linear light, then the
/// back half ([`canvas_linear_to_output_yuv`]) encodes the result.
///
/// # Errors
///
/// Returns [`Error::Geometry`] for non-even/zero canvas dimensions, or a color
/// error if a tile/canvas axis is unsupported/unresolved.
pub fn composite(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
) -> Result<Nv12Image> {
    composite_with(canvas_w, canvas_h, canvas, background, tiles, true)
}

/// Composite as [`composite`], but with explicit control over whether the
/// per-pixel EOTF/OETF run through the lookup-table path (`use_lut == true`,
/// ADR-0022) or the un-LUT'd transcendental **oracle** (`use_lut == false`).
///
/// [`composite`] calls this with `use_lut == true`; the oracle path exists so
/// equivalence tests can render the same canvas both ways (the LUT must match
/// the oracle within `±1` code value). The pipeline **order** (invariant #8) is
/// identical on both paths — only the EOTF/OETF *evaluation* changes.
///
/// # Errors
///
/// Same as [`composite`].
pub fn composite_with(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    use_lut: bool,
) -> Result<Nv12Image> {
    let n_threads = auto_thread_count();
    composite_with_threads(
        canvas_w, canvas_h, canvas, background, tiles, use_lut, n_threads,
    )
}

/// The number of worker threads to fan the composite across: the machine's
/// available parallelism, clamped to `[1, 64]`. Falls back to 1 when the count
/// is unavailable (the serial path).
fn auto_thread_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(64)
}

/// Below this many canvas pixels the thread-spawn overhead outweighs the win,
/// so the composite runs serially (also covers tiny placeholder/test canvases).
const PARALLEL_PIXEL_THRESHOLD: usize = 256 * 256;

/// Composite as [`composite_with`], but with an explicit worker-thread count.
///
/// The canvas is partitioned into **even-row-aligned bands** by chroma
/// row-pairs, so each worker owns whole UV rows of a disjoint `&mut` slice — the
/// split is race-free by construction (no shared mutable state on the data
/// plane) and the output is byte-identical regardless of `n_threads`, because
/// every band runs the identical deterministic per-pixel pipeline and rebases
/// the global row for tile addressing. `n_threads <= 1`, or a canvas below
/// [`PARALLEL_PIXEL_THRESHOLD`] pixels, renders serially.
///
/// `n_threads` is clamped to `[1, 64]` and to the number of chroma row-pairs
/// (`canvas_h / 2`) so a band is never empty.
///
/// # Errors
///
/// Same as [`composite_with`]; the first band error (in row order) is returned.
pub fn composite_with_threads(
    canvas_w: u32,
    canvas_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    use_lut: bool,
    n_threads: usize,
) -> Result<Nv12Image> {
    if canvas_w == 0 || canvas_h == 0 || canvas_w % 2 != 0 || canvas_h % 2 != 0 {
        return Err(Error::Geometry(format!(
            "canvas dimensions must be positive and even (got {canvas_w}x{canvas_h})"
        )));
    }
    let w = usize::try_from(canvas_w)
        .map_err(|_| Error::Geometry("canvas width overflow".to_owned()))?;
    let h = usize::try_from(canvas_h)
        .map_err(|_| Error::Geometry("canvas height overflow".to_owned()))?;

    // Build the transfer LUTs once per composite for exactly the transfers
    // present (tiles + canvas); empty when `use_lut` is false. Unsupported
    // transfers are absent and fall back to the oracle (same Err).
    let luts = if use_lut {
        let mut transfers: Vec<TransferCharacteristic> = vec![canvas.transfer];
        for tile in tiles {
            transfers.push(tile.image.color().transfer);
        }
        Some(LutSet::for_transfers(transfers))
    } else {
        None
    };

    let mut y_plane = vec![0_u8; w * h];
    let mut uv_plane = vec![0_u8; w * h / 2];

    // Total chroma row-pairs; clamp the worker count so no band is empty.
    let total_pairs = h / 2;
    let workers = n_threads.clamp(1, 64).min(total_pairs.max(1));

    if workers <= 1 || w.saturating_mul(h) < PARALLEL_PIXEL_THRESHOLD {
        // Serial path: one band covering the whole canvas.
        composite_band(
            &mut y_plane,
            &mut uv_plane,
            w,
            0,
            canvas_h,
            canvas,
            background,
            tiles,
            luts.as_ref(),
        )?;
    } else {
        composite_parallel(
            &mut y_plane,
            &mut uv_plane,
            w,
            total_pairs,
            workers,
            canvas,
            background,
            tiles,
            luts.as_ref(),
        )?;
    }

    Nv12Image::new(canvas_w, canvas_h, y_plane, uv_plane, canvas.output_tag())
}

/// Fan the composite across `workers` scoped threads over even-row-aligned
/// bands. `total_pairs` is `canvas_h / 2`; each band owns `pairs_per_band`
/// chroma row-pairs (= `2 * pairs_per_band` luma rows) of a **disjoint** `&mut`
/// slice of the Y and UV planes, so there is no shared mutable state and the
/// borrow checker proves the split race-free. Returns the first band error in
/// row order (deterministic).
#[allow(clippy::too_many_arguments)]
// reason: the band slices plus the shared composite parameters; see
// `composite_band`. A struct would obscure the disjoint-`&mut` band ownership.
fn composite_parallel(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    w: usize,
    total_pairs: usize,
    workers: usize,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    luts: Option<&LutSet>,
) -> Result<()> {
    // Even split of chroma row-pairs across workers (ceil), so every band is the
    // same size except possibly the last — which is exactly what `chunks_mut`
    // produces. The number of resulting bands may be < `workers`.
    let pairs_per_band = total_pairs.div_ceil(workers.max(1)).max(1);
    let y_band_len = pairs_per_band * 2 * w; // 2 luma rows per chroma pair
    let uv_band_len = pairs_per_band * w; // 1 interleaved UV row per chroma pair

    let y_bands = y_plane.chunks_mut(y_band_len);
    let uv_bands = uv_plane.chunks_mut(uv_band_len);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for (band_index, (band_y, band_uv)) in y_bands.zip(uv_bands).enumerate() {
            // Global top row of this band (luma rows): band_index * band height.
            // `pairs_per_band * 2` luma rows per band; the band's own height is
            // derived from its (possibly shorter, final) slice length.
            let py_start = u32::try_from(band_index * pairs_per_band * 2).unwrap_or(u32::MAX);
            let band_h = u32::try_from(band_y.len() / w.max(1)).unwrap_or(0);
            let handle = scope.spawn(move || {
                composite_band(
                    band_y, band_uv, w, py_start, band_h, canvas, background, tiles, luts,
                )
            });
            handles.push(handle);
        }
        // Join in spawn (row) order and return the first error encountered.
        let mut first_err: Option<Error> = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
                Err(_) => {
                    if first_err.is_none() {
                        first_err = Some(Error::Geometry(
                            "composite worker thread panicked".to_owned(),
                        ));
                    }
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    })
}

/// Render the canvas rows `[py_start, py_start + band_h)` into band-local Y/UV
/// planes (`band_y` is `band_h` rows of `w` bytes; `band_uv` is `band_h/2` rows
/// of `w` bytes). The tile/canvas coordinate space is the **global** canvas;
/// `py_start` rebases the band-local row index to the global row used for tile
/// addressing.
///
/// This is the single per-pixel pipeline used by both the serial path and the
/// parallel bands: it runs the front half (`tile_yuv_to_canvas_linear`), the
/// premultiplied linear `over`, and the back half (`canvas_linear_to_output_yuv`)
/// — via the LUT when `luts` is `Some`, else the oracle.
#[allow(clippy::too_many_arguments)]
// reason: this is the internal band kernel; the arguments are the band slices
// plus the shared composite parameters. Grouping them into a struct would not
// reduce the surface and would obscure the disjoint-`&mut` band ownership that
// makes the parallel split race-free.
fn composite_band(
    band_y: &mut [u8],
    band_uv: &mut [u8],
    w: usize,
    py_start: u32,
    band_h: u32,
    canvas: CanvasColor,
    background: LinearRgba,
    tiles: &[Tile<'_>],
    luts: Option<&LutSet>,
) -> Result<()> {
    let bg_premul = background.premultiplied();
    for py_local in 0..band_h {
        let py = py_start.saturating_add(py_local);
        for px in 0..u32::try_from(w).unwrap_or(u32::MAX) {
            // Fold the covering tiles back-to-front in linear light.
            let mut acc = bg_premul;
            for tile in tiles {
                let Some((sx, sy)) = tile_local_coords(tile, px, py) else {
                    continue;
                };
                let Some((y8, cb8, cr8)) = tile.image.sample(sx, sy) else {
                    continue;
                };
                let lin = match luts {
                    Some(lut) => {
                        lut.tile_yuv_to_canvas_linear(y8, cb8, cr8, tile.image.color(), canvas)?
                    }
                    None => tile_yuv_to_canvas_linear(y8, cb8, cr8, tile.image.color(), canvas)?,
                };
                let src = LinearRgba {
                    r: lin[0],
                    g: lin[1],
                    b: lin[2],
                    a: tile.opacity.clamp(0.0, 1.0),
                }
                .premultiplied();
                acc = over(src, acc);
            }
            let straight = PremulRgba {
                r: acc.r,
                g: acc.g,
                b: acc.b,
                a: acc.a,
            }
            .unpremultiplied();
            let out = match luts {
                Some(lut) => {
                    lut.canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?
                }
                None => canvas_linear_to_output_yuv([straight.r, straight.g, straight.b], canvas)?,
            };
            write_pixel(band_y, band_uv, w, px, py_local, out);
        }
    }
    Ok(())
}

/// Map a canvas pixel to the tile-local coordinate it samples, or [`None`] if
/// the pixel is outside the tile's placed rectangle.
fn tile_local_coords(tile: &Tile<'_>, px: u32, py: u32) -> Option<(u32, u32)> {
    let sx = px.checked_sub(tile.dst_x)?;
    let sy = py.checked_sub(tile.dst_y)?;
    if sx >= tile.image.width() || sy >= tile.image.height() {
        return None;
    }
    Some((sx, sy))
}

/// Write one output pixel's `(y, cb, cr)` into the NV12 planes. Chroma is
/// written for every pixel (last-writer-wins within a 2x2 block, matching the
/// reference's nearest-neighbour model).
fn write_pixel(y_plane: &mut [u8], uv_plane: &mut [u8], w: usize, px: u32, py: u32, yuv: [u8; 3]) {
    let (Ok(xi), Ok(yi)) = (usize::try_from(px), usize::try_from(py)) else {
        return;
    };
    if let Some(slot) = y_plane.get_mut(yi * w + xi) {
        *slot = yuv[0];
    }
    let cx = xi / 2;
    let cy = yi / 2;
    let uv_index = (cy * w) + (cx * 2);
    if let Some(slot) = uv_plane.get_mut(uv_index) {
        *slot = yuv[1];
    }
    if let Some(slot) = uv_plane.get_mut(uv_index + 1) {
        *slot = yuv[2];
    }
}
