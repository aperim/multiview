//! Build the **overlay draw-data** that the `run` paths bake into the
//! composited program (feature `overlay`).
//!
//! This wires the pure overlay *models* ([`mosaic_overlay`]) and the pure-DSP
//! meter draw-data ([`mosaic_audio`]) into the compositor's overlay primitive
//! model ([`mosaic_compositor::overlay`]), then rasterizes text with the
//! stage-1 text engine. The engine drive loop and the libav pipeline call
//! [`OverlayBaker::draw_list`] **off the hot path** (on the collected output
//! frame, after the protected output core has emitted it) and bake the result
//! with [`mosaic_compositor::overlay::apply_overlays_to_nv12`] — so overlay
//! rasterization can never block or back-pressure the output clock (inv #1/#10).
//!
//! The set wired here is a representative operator surface — a wall-clock label,
//! a peak-held dB meter, the SMPTE safe-area graticules, a tally border, and the
//! active burned-in subtitle cue — so a `run --features ffmpeg,overlay` produces
//! a real, overlaid, playable file. It is intentionally config-light: a fuller
//! config→overlay-stack resolver is a follow-up; the contract proven here is
//! that configured overlays reach actual output pixels.

use mosaic_audio::meterdata::{Conflator, MeterSample};
use mosaic_compositor::overlay::meters::{MeterBar, MeterScale};
use mosaic_compositor::overlay::subpass::{
    OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect,
};
use mosaic_compositor::overlay::text::{FontFamily, TextEngine};
use mosaic_compositor::Error as CompositorError;
use mosaic_core::time::MediaTime;
use mosaic_overlay::clock::{
    ClockFace, ClockModel, RefSource, RefStatus, TimeRef, TimeZoneOffset, WallTime,
};
use mosaic_overlay::resolve::CanvasSize;
use mosaic_overlay::safearea::{SafeAreaKind, SafeAreaMarkers};
use mosaic_overlay::subtitle::CueTrack;

/// Opaque white / amber / cyan linear overlay colours for the baked surface.
const WHITE: OverlayColor = OverlayColor::opaque(0.85, 0.85, 0.85);
const AMBER: OverlayColor = OverlayColor::opaque(0.95, 0.6, 0.05);
const GREEN: OverlayColor = OverlayColor::opaque(0.1, 0.85, 0.2);
const SAFE: OverlayColor = OverlayColor::new(0.9, 0.9, 0.9, 0.7);
const TALLY_RED: OverlayColor = OverlayColor::opaque(0.85, 0.05, 0.05);

/// Builds the per-frame overlay draw-data for a run.
///
/// Holds the (single) text engine and the conflated meter + peak-hold state so
/// re-rendering an unchanged label re-rasterizes nothing (the engine's per-glyph
/// atlas). Constructed once per run, then queried per collected output frame.
pub struct OverlayBaker {
    engine: TextEngine,
    canvas: CanvasSize,
    clock: ClockModel,
    meter: MeterBar,
    conflator: Conflator<MeterSample>,
    subtitles: Option<CueTrack>,
    base_unix_secs: i64,
}

impl OverlayBaker {
    /// Build a baker for a `width`×`height` canvas.
    ///
    /// `subtitles` is an optional parsed SRT/VTT track whose active cue is
    /// burned in; `base_unix_secs` anchors the wall-clock label so the rendered
    /// time advances with the media timeline deterministically.
    ///
    /// # Errors
    ///
    /// Returns the compositor [`CompositorError`] if the bundled OFL fonts fail
    /// to load.
    pub fn new(
        width: u32,
        height: u32,
        subtitles: Option<CueTrack>,
        base_unix_secs: i64,
    ) -> Result<Self, CompositorError> {
        Ok(Self {
            engine: TextEngine::new()?,
            canvas: CanvasSize::new(width, height),
            clock: ClockModel::new(
                ClockFace::digital_24h(),
                TimeZoneOffset::UTC,
                TimeRef::new(RefSource::System, RefStatus::Freerun),
            ),
            meter: MeterBar::new(MeterScale::default()),
            conflator: Conflator::with_rate(mosaic_audio::meterdata::DISPLAY_HZ),
            subtitles,
            base_unix_secs,
        })
    }

    /// Feed the latest meter reading (dBFS) into the conflator; the next
    /// [`Self::draw_list`] at or past a display interval picks it up.
    pub fn observe_meter_db(&mut self, db: f64) {
        self.conflator.accept(MeterSample { db });
    }

    /// Build the overlay draw-data for the output instant `pts`.
    ///
    /// Renders: a wall-clock `HH:MM:SS` label (top-left), a vertical peak-held dB
    /// meter (right edge), the action/title safe-area graticules + center cross,
    /// a red program tally border, and the active subtitle cue (bottom-centre).
    /// Re-rasterizes text only when its content changed (per-glyph atlas, T2).
    ///
    /// # Errors
    ///
    /// Returns the compositor [`CompositorError`] only if a glyph cannot fit the
    /// atlas (a degenerate font/size); text that simply does not ink is skipped.
    pub fn draw_list(&mut self, pts: MediaTime) -> Result<OverlayDrawList, CompositorError> {
        let mut list = OverlayDrawList::new();
        let (w, h) = (self.canvas.width, self.canvas.height);

        // Safe-area graticules + center cross (drawn first / furthest back).
        let markers = SafeAreaMarkers::default()
            .with_kind(SafeAreaKind::ActionSafe, true)
            .with_kind(SafeAreaKind::TitleSafe, true)
            .with_center_cross(true)
            .resolve(self.canvas);
        for safe in &markers.rects {
            push_box_outline(&mut list, safe.rect, 2, SAFE);
        }
        if let Some(cross) = markers.center_cross {
            push_center_cross(&mut list, cross, 2, SAFE);
        }

        // A 6px red program tally border around the whole canvas.
        push_box_outline(
            &mut list,
            mosaic_overlay::geometry::PixelRect {
                x: 0.0,
                y: 0.0,
                width: f32_dim(w),
                height: f32_dim(h),
            },
            6,
            TALLY_RED,
        );

        // The wall-clock label, top-left (proportional Sans is fine; Mono keeps
        // digit columns aligned — use Mono for the timecode).
        let secs = self
            .base_unix_secs
            .saturating_add(pts.as_nanos() / 1_000_000_000);
        if let Some(text) = self.clock.render_digital(WallTime::from_unix_seconds(secs)) {
            self.push_text(
                &mut list,
                &text,
                TextRun {
                    family: FontFamily::Mono,
                    size_px: 28.0,
                    x: 12,
                    y: 8,
                    color: WHITE,
                },
            )?;
        }

        // The conflated dB meter as a vertical bar down the right edge.
        if let Some(sample) = self.conflator.poll(pts.as_nanos()) {
            self.meter.observe_db(sample.db_f32());
        }
        self.meter.decay_peak(0.02);
        let track = OverlayRect::new(i32_dim(w.saturating_sub(28)), 40, 16, h.saturating_sub(80));
        self.meter.push_into(&mut list, track, true, GREEN, AMBER);

        // The active subtitle cue, burned in bottom-centre. Clone the active
        // cue's lines so the immutable borrow of `self.subtitles` ends before the
        // mutable `push_text` borrows `self.engine`.
        let cue_lines: Vec<String> = self
            .subtitles
            .as_ref()
            .and_then(|track| track.active_cue(pts))
            .map(|cue| cue.lines.clone())
            .unwrap_or_default();
        if !cue_lines.is_empty() {
            let size = 30.0;
            let y = i32_dim(h.saturating_sub(80));
            for (i, line) in cue_lines.iter().enumerate() {
                let line_y = y.saturating_add(i32_dim(u32_from_usize(i).saturating_mul(36)));
                let approx_w =
                    u32_from_usize(line.chars().count()).saturating_mul(quantize_advance(size));
                let x = i32_dim(w.saturating_sub(approx_w) / 2);
                self.push_text(
                    &mut list,
                    line,
                    TextRun {
                        family: FontFamily::Sans,
                        size_px: size,
                        x,
                        y: line_y,
                        color: AMBER,
                    },
                )?;
            }
        }

        Ok(list)
    }

    /// Shape `text` per `run` and append its glyph quads to `list`, tinted by the
    /// run colour (the baseline sits at the run origin; the engine offsets each
    /// glyph). Re-rasterizes only on a content change (per-glyph atlas, T2).
    fn push_text(
        &mut self,
        list: &mut OverlayDrawList,
        text: &str,
        run: TextRun,
    ) -> Result<(), CompositorError> {
        let color = run.color;
        let rasterized = self.engine.rasterize_run(
            text,
            run.family,
            run.size_px,
            [color.r, color.g, color.b, color.a],
        )?;
        for glyph in rasterized.glyphs() {
            list.push(OverlayPrimitive::Glyph {
                dest_x: run.x.saturating_add(glyph.dest_x),
                dest_y: run.y.saturating_add(glyph.dest_y),
                width: glyph.width,
                height: glyph.height,
                coverage: glyph
                    .premultiplied_rgba
                    .chunks_exact(4)
                    .filter_map(|px| px.get(3).copied())
                    .collect(),
                color,
            });
        }
        Ok(())
    }
}

/// One placed text run: where its baseline-left origin sits on the canvas, the
/// bundled face, the pixel size, and the linear tint.
#[derive(Debug, Clone, Copy)]
struct TextRun {
    family: FontFamily,
    size_px: f32,
    x: i32,
    y: i32,
    color: OverlayColor,
}

/// Append four line strokes forming the outline of a [`PixelRect`] at the given
/// thickness.
fn push_box_outline(
    list: &mut OverlayDrawList,
    rect: mosaic_overlay::geometry::PixelRect,
    thickness: u32,
    color: OverlayColor,
) {
    let left = round_dim(rect.x);
    let top = round_dim(rect.y);
    let width = round_dim(rect.width);
    let height = round_dim(rect.height);
    if width == 0 || height == 0 {
        return;
    }
    let thick = thickness.max(1).min(width / 2).min(height / 2).max(1);
    let (xi, yi) = (i32_dim(left), i32_dim(top));
    let bottom = yi.saturating_add(i32_dim(height.saturating_sub(thick)));
    let right = xi.saturating_add(i32_dim(width.saturating_sub(thick)));
    // top, bottom, left, right strokes.
    list.push(line(xi, yi, width, thick, color));
    list.push(line(xi, bottom, width, thick, color));
    list.push(line(xi, yi, thick, height, color));
    list.push(line(right, yi, thick, height, color));
}

/// Append a center-cross marker (two short strokes through the raster centre).
fn push_center_cross(
    list: &mut OverlayDrawList,
    cross: mosaic_overlay::safearea::CenterCross,
    thickness: u32,
    color: OverlayColor,
) {
    let arm = round_dim(cross.arm_px).max(1);
    let cx = round_dim(cross.x);
    let cy = round_dim(cross.y);
    let t = thickness.max(1);
    // Horizontal arm.
    list.push(line(
        i32_dim(cx.saturating_sub(arm)),
        i32_dim(cy.saturating_sub(t / 2)),
        arm.saturating_mul(2),
        t,
        color,
    ));
    // Vertical arm.
    list.push(line(
        i32_dim(cx.saturating_sub(t / 2)),
        i32_dim(cy.saturating_sub(arm)),
        t,
        arm.saturating_mul(2),
        color,
    ));
}

/// An axis-aligned line/border primitive.
fn line(x: i32, y: i32, width: u32, height: u32, color: OverlayColor) -> OverlayPrimitive {
    OverlayPrimitive::Line {
        rect: OverlayRect::new(x, y, width, height),
        color,
    }
}

/// A coarse per-glyph advance estimate for centring text (px); good enough for
/// placement (exact centring would re-measure the run, unnecessary here).
fn quantize_advance(size_px: f32) -> u32 {
    round_dim(size_px * 0.55).max(1)
}

/// Round a non-negative `f32` pixel measure to `u32` (saturating), no `as`.
fn round_dim(value: f32) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let rounded = value.round();
    let mut lo = 0_u32;
    let mut hi = u32::MAX;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f32_dim(mid) <= rounded {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Exact small-`u32` → `f32`, no `as`.
fn f32_dim(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Saturating `u32` → `i32`, no `as`.
fn i32_dim(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Saturating `usize` → `u32`, no `as`.
fn u32_from_usize(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
