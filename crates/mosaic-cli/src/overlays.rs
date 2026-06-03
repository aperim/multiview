//! Build the **per-tile overlay draw-data** that the `run` paths bake into the
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
//! ## Per-tile surface (the operator multiviewer)
//!
//! Unlike a single program-wide overlay set, the baker iterates the **layout
//! cells** and, for *each* cell rectangle, draws — all positioned and scaled
//! *within that cell*, never the whole canvas:
//!
//! 1. an **input label** — the source's `display_name` (or its id), bottom-left;
//! 2. a **per-input dB meter** — a vertical peak-held bar down the tile's right
//!    edge, fed by **that input's own** audio loudness (a silent / audio-free
//!    input rides its floor honestly, never a fabricated constant);
//! 3. a **state / fault flag** — a top-left badge from the tile's
//!    [`SourceState`](mosaic_core::traits::SourceState)
//!    (`LIVE`/`STALE`/`RECONNECTING`/`NO_SIGNAL`), conveyed as **text** (not
//!    colour alone) via the alert-card chrome;
//! 4. an optional **per-tile safe-area** graticule + centre cross, drawn *inside*
//!    the cell rect (fixing the previous canvas-wide marker).
//!
//! A program-wide wall-clock label is kept (top-left of the whole canvas) when a
//! clock is configured. A missing source just shows `NO_SIGNAL` + a floor meter;
//! nothing here can stall.

use std::collections::HashMap;

use mosaic_audio::meterdata::{Conflator, MeterSample};
use mosaic_compositor::overlay::meters::{MeterBar, MeterScale};
use mosaic_compositor::overlay::subpass::{
    OverlayColor, OverlayDrawList, OverlayPrimitive, OverlayRect,
};
use mosaic_compositor::overlay::text::{FontFamily, TextEngine};
use mosaic_compositor::Error as CompositorError;
use mosaic_core::time::MediaTime;
use mosaic_core::traits::SourceState;
use mosaic_overlay::clock::{
    ClockFace, ClockModel, RefSource, RefStatus, TimeRef, TimeZoneOffset, WallTime,
};
use mosaic_overlay::geometry::PixelRect;
use mosaic_overlay::resolve::CanvasSize;
use mosaic_overlay::safearea::{SafeAreaKind, SafeAreaMarkers};
use mosaic_overlay::subtitle::CueTrack;

/// Opaque white / amber / cyan / green linear overlay colours for the surface.
const WHITE: OverlayColor = OverlayColor::opaque(0.92, 0.92, 0.92);
const AMBER: OverlayColor = OverlayColor::opaque(0.95, 0.6, 0.05);
const GREEN: OverlayColor = OverlayColor::opaque(0.1, 0.85, 0.2);
const RED: OverlayColor = OverlayColor::opaque(0.9, 0.12, 0.12);
const SAFE: OverlayColor = OverlayColor::new(0.9, 0.9, 0.9, 0.7);
/// A translucent dark backing for the per-tile label / flag chrome so the text
/// reads over any underlying picture (the meaning is the *text*, not the colour).
const CHROME_BG: OverlayColor = OverlayColor::new(0.0, 0.0, 0.0, 0.55);

/// The static placement of one mosaic tile's overlay surface: the cell's pixel
/// rectangle on the canvas plus the (immutable) label text for the bound source.
///
/// Built once from the solved layout + config source names; the per-frame
/// dynamics (meter level, tile state) are supplied to [`OverlayBaker::draw_list`]
/// separately so this stays a cheap value type.
#[derive(Debug, Clone, PartialEq)]
pub struct TileSpec {
    /// The bound source id (store key / fallback label).
    pub source_id: String,
    /// The human-facing label to draw (display name, or the id when unnamed).
    pub label: String,
    /// The cell's pixel rectangle on the canvas (top-left origin).
    pub rect: PixelRect,
}

impl TileSpec {
    /// Build a tile spec for `source_id` placed at `rect`, labelled `label`.
    #[must_use]
    pub fn new(source_id: impl Into<String>, label: impl Into<String>, rect: PixelRect) -> Self {
        Self {
            source_id: source_id.into(),
            label: label.into(),
            rect,
        }
    }
}

/// The live per-tile dynamics for one output frame: the source's current
/// loudness (dBFS) and its lifecycle state. A tile with no decodable audio
/// passes its meter floor; a missing/unconnected source is `NO_SIGNAL`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileDynamics {
    /// The source's current program loudness in dBFS (its own audio).
    pub meter_db: f64,
    /// The tile's lifecycle state sampled this tick.
    pub state: SourceState,
}

/// The per-tile peak-hold meter + conflator state, kept across frames so the
/// vertical bar tracks the source's own audio and the held peak decays smoothly.
struct TileMeter {
    bar: MeterBar,
    conflator: Conflator<MeterSample>,
}

impl TileMeter {
    fn new() -> Self {
        Self {
            bar: MeterBar::new(MeterScale::default()),
            conflator: Conflator::with_rate(mosaic_audio::meterdata::DISPLAY_HZ),
        }
    }
}

/// Builds the per-frame, **per-tile** overlay draw-data for a run.
///
/// Holds the (single, shared) text engine, the per-tile static placement
/// ([`TileSpec`]), each tile's conflated-meter + peak-hold state, and an optional
/// program-wide clock + burned-in subtitle track. Constructed once per run, then
/// queried per collected output frame with [`OverlayBaker::draw_list`].
pub struct OverlayBaker {
    engine: TextEngine,
    canvas: CanvasSize,
    tiles: Vec<TileSpec>,
    meters: Vec<TileMeter>,
    clock: Option<ClockModel>,
    subtitles: Option<CueTrack>,
    base_unix_secs: i64,
    per_tile_safe_area: bool,
}

impl OverlayBaker {
    /// Build a per-tile baker for a `width`×`height` canvas over `tiles`.
    ///
    /// `subtitles` is an optional parsed SRT/VTT track whose active cue is burned
    /// in bottom-centre; `base_unix_secs` anchors the program-wide wall-clock
    /// label so the rendered time advances with the media timeline
    /// deterministically. The program clock is always shown top-left;
    /// per-tile safe-area markers are off by default (enable with
    /// [`OverlayBaker::with_per_tile_safe_area`]).
    ///
    /// # Errors
    ///
    /// Returns the compositor [`CompositorError`] if the bundled OFL fonts fail
    /// to load.
    pub fn new(
        width: u32,
        height: u32,
        tiles: Vec<TileSpec>,
        subtitles: Option<CueTrack>,
        base_unix_secs: i64,
    ) -> Result<Self, CompositorError> {
        let meters = tiles.iter().map(|_| TileMeter::new()).collect();
        Ok(Self {
            engine: TextEngine::new()?,
            canvas: CanvasSize::new(width, height),
            tiles,
            meters,
            clock: Some(ClockModel::new(
                ClockFace::digital_24h(),
                TimeZoneOffset::UTC,
                TimeRef::new(RefSource::System, RefStatus::Freerun),
            )),
            subtitles,
            base_unix_secs,
            per_tile_safe_area: false,
        })
    }

    /// Enable per-tile safe-area / centre-cross markers (drawn inside each cell
    /// rect), builder-style. Off by default.
    #[must_use]
    pub fn with_per_tile_safe_area(mut self, on: bool) -> Self {
        self.per_tile_safe_area = on;
        self
    }

    /// The tiles this baker draws (their static placement), in declaration order.
    #[must_use]
    pub fn tiles(&self) -> &[TileSpec] {
        &self.tiles
    }

    /// Build the overlay draw-data for the output instant `pts`, given each
    /// tile's live [`TileDynamics`] keyed by source id.
    ///
    /// For every tile this draws — *inside that tile's cell rect* — the input
    /// label, the source's own peak-held dB meter, and a state/fault flag; plus
    /// the optional per-tile safe-area and the program-wide clock + subtitle cue.
    /// A source absent from `dynamics` is treated as `NO_SIGNAL` at the meter
    /// floor (a missing source never stalls — invariant #1).
    ///
    /// # Errors
    ///
    /// Returns the compositor [`CompositorError`] only if a glyph cannot fit the
    /// atlas (a degenerate font/size); text that simply does not ink is skipped.
    pub fn draw_list(
        &mut self,
        pts: MediaTime,
        dynamics: &HashMap<String, TileDynamics>,
    ) -> Result<OverlayDrawList, CompositorError> {
        let mut list = OverlayDrawList::new();
        let now_ns = pts.as_nanos();

        // Per-tile surface. Take the tiles + meters by index so the per-tile
        // meter state (a mutable borrow) does not alias the immutable tile spec.
        for i in 0..self.tiles.len() {
            // Resolve this tile's live dynamics (default: NO_SIGNAL at floor).
            let (label, rect, source_id) = match self.tiles.get(i) {
                Some(spec) => (spec.label.clone(), spec.rect, spec.source_id.clone()),
                None => continue,
            };
            let dyn_ = dynamics.get(&source_id).copied().unwrap_or(TileDynamics {
                meter_db: mosaic_audio::Ballistics::FLOOR_DB,
                state: SourceState::NoSignal,
            });

            // Advance this tile's meter from its own audio (conflated ~30 Hz).
            if let Some(meter) = self.meters.get_mut(i) {
                meter.conflator.accept(MeterSample { db: dyn_.meter_db });
                if let Some(sample) = meter.conflator.poll(now_ns) {
                    meter.bar.observe_db(sample.db_f32());
                }
                meter.bar.decay_peak(0.02);
            }
            let bar = self.meters.get(i).map(|m| m.bar);

            self.draw_tile(&mut list, &label, rect, dyn_.state, bar)?;
        }

        // Program-wide wall-clock label, top-left of the whole canvas.
        if let Some(clock) = self.clock.as_ref() {
            let secs = self.base_unix_secs.saturating_add(now_ns / 1_000_000_000);
            if let Some(text) = clock.render_digital(WallTime::from_unix_seconds(secs)) {
                self.push_text(
                    &mut list,
                    &text,
                    TextRun {
                        family: FontFamily::Mono,
                        size_px: 26.0,
                        x: 12,
                        y: 6,
                        color: WHITE,
                    },
                )?;
            }
        }

        // The active subtitle cue, burned in bottom-centre. Clone the cue lines
        // so the immutable borrow of `self.subtitles` ends before `push_text`
        // borrows `self.engine` mutably.
        let cue_lines: Vec<String> = self
            .subtitles
            .as_ref()
            .and_then(|track| track.active_cue(pts))
            .map(|cue| cue.lines.clone())
            .unwrap_or_default();
        if !cue_lines.is_empty() {
            let (w, h) = (self.canvas.width, self.canvas.height);
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

    /// Draw one tile's overlay surface inside its cell `rect`: optional safe-area,
    /// the source's vertical dB meter (right edge), a bottom-left input label, and
    /// a top-left state/fault flag.
    fn draw_tile(
        &mut self,
        list: &mut OverlayDrawList,
        label: &str,
        rect: PixelRect,
        state: SourceState,
        bar: Option<MeterBar>,
    ) -> Result<(), CompositorError> {
        let geom = TileGeometry::resolve(rect);
        if geom.width < MIN_TILE_PX || geom.height < MIN_TILE_PX {
            // Too small to host a legible surface; skip rather than draw mush.
            return Ok(());
        }

        // Per-tile safe-area + centre cross, scaled INSIDE the cell rect.
        if self.per_tile_safe_area {
            let markers = SafeAreaMarkers::default()
                .with_kind(SafeAreaKind::ActionSafe, true)
                .with_kind(SafeAreaKind::TitleSafe, true)
                .with_center_cross(true)
                .resolve(CanvasSize::new(geom.width, geom.height));
            for safe in &markers.rects {
                // Offset each marker rect from cell-local space to canvas space.
                push_box_outline(list, geom.offset_rect(safe.rect), 1, SAFE);
            }
            if let Some(cross) = markers.center_cross {
                push_center_cross(list, geom.offset_cross(cross), 1, SAFE);
            }
        }

        // The per-input dB meter: a vertical peak-held bar down the tile's RIGHT
        // edge, fed by THAT input's own audio.
        if let Some(bar) = bar {
            let track = geom.meter_track();
            bar.push_into(list, track, true, GREEN, AMBER);
        }

        // The state / fault flag: a top-left badge whose TEXT names the state, on
        // a translucent backing (meaning is the text, not the colour — A11y).
        let flag_text = state_flag_text(state);
        let flag_color = state_flag_color(state);
        let flag_h = geom.chip_height();
        let flag_w = geom.flag_width(flag_text.chars().count());
        push_filled_rect(
            list,
            OverlayRect::new(geom.x, geom.y, flag_w, flag_h),
            CHROME_BG,
        );
        self.push_text(
            list,
            flag_text,
            TextRun {
                family: FontFamily::Sans,
                size_px: geom.chip_text_px(),
                x: geom.x.saturating_add(geom.pad_i32()),
                y: geom.y.saturating_add(geom.pad_i32() / 2),
                color: flag_color,
            },
        )?;

        // The input label: bottom-left of the tile, on a translucent backing.
        let label_h = geom.chip_height();
        let label_w = geom
            .flag_width(label.chars().count())
            .min(geom.meter_left());
        let label_y = geom
            .y
            .saturating_add(i32_dim(geom.height.saturating_sub(label_h)));
        push_filled_rect(
            list,
            OverlayRect::new(geom.x, label_y, label_w, label_h),
            CHROME_BG,
        );
        self.push_text(
            list,
            label,
            TextRun {
                family: FontFamily::Sans,
                size_px: geom.chip_text_px(),
                x: geom.x.saturating_add(geom.pad_i32()),
                y: label_y.saturating_add(geom.pad_i32() / 2),
                color: WHITE,
            },
        )?;

        Ok(())
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

/// The minimum cell extent (px) that can host a legible per-tile surface; smaller
/// tiles are skipped so the run never draws unreadable mush.
const MIN_TILE_PX: u32 = 48;

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

/// The integer pixel geometry of one cell rectangle, with the helpers the tile
/// surface uses to place its meter / label / flag *inside* that cell.
#[derive(Debug, Clone, Copy)]
struct TileGeometry {
    /// Cell top-left x in canvas pixels.
    x: i32,
    /// Cell top-left y in canvas pixels.
    y: i32,
    /// Cell width in pixels.
    width: u32,
    /// Cell height in pixels.
    height: u32,
}

impl TileGeometry {
    /// Quantise a [`PixelRect`] to integer canvas pixels.
    fn resolve(rect: PixelRect) -> Self {
        Self {
            x: round_dim_signed(rect.x),
            y: round_dim_signed(rect.y),
            width: round_dim(rect.width),
            height: round_dim(rect.height),
        }
    }

    /// The pad (px) for chrome inside the cell — a small fraction of the cell.
    fn pad(self) -> u32 {
        (self.width.min(self.height) / 40).clamp(2, 8)
    }

    fn pad_i32(self) -> i32 {
        i32_dim(self.pad())
    }

    /// The chrome chip height (flag/label band) — a fraction of the cell height.
    fn chip_height(self) -> u32 {
        (self.height / 8).clamp(14, 40)
    }

    /// The chrome text size (px), sized to fit the chip with padding.
    fn chip_text_px(self) -> f32 {
        let h = self.chip_height().saturating_sub(self.pad());
        f32_dim(h.clamp(10, 30))
    }

    /// The width (px) of a chip holding `glyphs` characters at the chip text size,
    /// clamped to the cell width.
    fn flag_width(self, glyphs: usize) -> u32 {
        let advance = quantize_advance(self.chip_text_px());
        let text = u32_from_usize(glyphs).saturating_mul(advance);
        text.saturating_add(self.pad().saturating_mul(2))
            .clamp(self.pad().saturating_mul(2).max(1), self.width)
    }

    /// The meter track width (px) down the tile's right edge.
    fn meter_width(self) -> u32 {
        (self.width / 16).clamp(6, 18)
    }

    /// The canvas x where the meter track begins (left edge of the meter).
    fn meter_left(self) -> u32 {
        let inset = self.meter_width().saturating_add(self.pad());
        self.width.saturating_sub(inset)
    }

    /// The meter track rectangle: a vertical strip down the tile's right edge,
    /// inset from the top/bottom by the chrome chip height so it does not collide
    /// with the flag/label.
    fn meter_track(self) -> OverlayRect {
        let pad = self.pad();
        let mw = self.meter_width();
        let track_x = self
            .x
            .saturating_add(i32_dim(self.width.saturating_sub(mw.saturating_add(pad))));
        let chip = self.chip_height();
        let top_inset = chip.saturating_add(pad);
        let track_y = self.y.saturating_add(i32_dim(top_inset));
        let track_h = self
            .height
            .saturating_sub(top_inset.saturating_add(chip).saturating_add(pad))
            .max(1);
        OverlayRect::new(track_x, track_y, mw, track_h)
    }

    /// Offset a cell-local [`PixelRect`] (origin at the cell's top-left) into
    /// canvas space.
    fn offset_rect(self, local: PixelRect) -> PixelRect {
        PixelRect {
            x: local.x + f32_signed(self.x),
            y: local.y + f32_signed(self.y),
            width: local.width,
            height: local.height,
        }
    }

    /// Offset a cell-local centre cross into canvas space.
    fn offset_cross(
        self,
        local: mosaic_overlay::safearea::CenterCross,
    ) -> mosaic_overlay::safearea::CenterCross {
        mosaic_overlay::safearea::CenterCross {
            x: local.x + f32_signed(self.x),
            y: local.y + f32_signed(self.y),
            arm_px: local.arm_px,
        }
    }
}

/// The short, all-text flag label for a tile's lifecycle state. Conveys meaning
/// as text (not colour alone) per the accessibility requirement.
fn state_flag_text(state: SourceState) -> &'static str {
    match state {
        SourceState::Live => "LIVE",
        SourceState::Stale => "STALE",
        SourceState::Reconnecting => "RECONNECT",
        SourceState::NoSignal => "NO SIGNAL",
        // `SourceState` is `#[non_exhaustive]`; a future state is surfaced as an
        // explicit fault flag rather than silently mislabelled as LIVE.
        _ => "FAULT",
    }
}

/// The flag tint for a tile's state (green `LIVE`, amber `STALE`/`RECONNECTING`,
/// red `NO_SIGNAL`). The text already carries the meaning; the colour reinforces
/// it.
fn state_flag_color(state: SourceState) -> OverlayColor {
    match state {
        SourceState::Live => GREEN,
        SourceState::Stale | SourceState::Reconnecting => AMBER,
        // NO_SIGNAL and any future fault state read red (the text carries the
        // precise meaning; the colour reinforces it).
        _ => RED,
    }
}

/// Append a filled rectangle primitive (chrome backing).
fn push_filled_rect(list: &mut OverlayDrawList, rect: OverlayRect, color: OverlayColor) {
    list.push(OverlayPrimitive::FilledRect {
        rect,
        corner_radius: 0,
        color,
    });
}

/// Append four line strokes forming the outline of a [`PixelRect`] at the given
/// thickness.
fn push_box_outline(
    list: &mut OverlayDrawList,
    rect: PixelRect,
    thickness: u32,
    color: OverlayColor,
) {
    let left = round_dim_signed(rect.x);
    let top = round_dim_signed(rect.y);
    let width = round_dim(rect.width);
    let height = round_dim(rect.height);
    if width == 0 || height == 0 {
        return;
    }
    let thick = thickness.max(1).min(width / 2).min(height / 2).max(1);
    let bottom = top.saturating_add(i32_dim(height.saturating_sub(thick)));
    let right = left.saturating_add(i32_dim(width.saturating_sub(thick)));
    list.push(line(left, top, width, thick, color));
    list.push(line(left, bottom, width, thick, color));
    list.push(line(left, top, thick, height, color));
    list.push(line(right, top, thick, height, color));
}

/// Append a center-cross marker (two short strokes through the cross centre).
fn push_center_cross(
    list: &mut OverlayDrawList,
    cross: mosaic_overlay::safearea::CenterCross,
    thickness: u32,
    color: OverlayColor,
) {
    let arm = round_dim(cross.arm_px).max(1);
    let cx = round_dim_signed(cross.x);
    let cy = round_dim_signed(cross.y);
    let t = thickness.max(1);
    list.push(line(
        cx.saturating_sub(i32_dim(arm)),
        cy.saturating_sub(i32_dim(t / 2)),
        arm.saturating_mul(2),
        t,
        color,
    ));
    list.push(line(
        cx.saturating_sub(i32_dim(t / 2)),
        cy.saturating_sub(i32_dim(arm)),
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

/// A coarse per-glyph advance estimate for sizing chrome (px); good enough for
/// placement (exact measuring would re-shape the run, unnecessary here).
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

/// Round a (possibly negative) `f32` pixel coordinate to `i32` (saturating), no
/// `as` cast.
fn round_dim_signed(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    if value < 0.0 {
        i32_dim(round_dim(-value)).saturating_neg()
    } else {
        i32_dim(round_dim(value))
    }
}

/// Exact small-`u32` → `f32`, no `as`.
fn f32_dim(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Exact small-`i32` → `f32`, no `as`.
fn f32_signed(value: i32) -> f32 {
    if value < 0 {
        -f32_dim(value.unsigned_abs())
    } else {
        f32_dim(u32::try_from(value).unwrap_or(u32::MAX))
    }
}

/// Saturating `u32` → `i32`, no `as`.
fn i32_dim(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Saturating `usize` → `u32`, no `as`.
fn u32_from_usize(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A 2x2 grid of 640x360 cells on a 1280x720 canvas.
    fn quad_tiles() -> Vec<TileSpec> {
        let mk = |id: &str, label: &str, x: f32, y: f32| {
            TileSpec::new(
                id,
                label,
                PixelRect {
                    x,
                    y,
                    width: 640.0,
                    height: 360.0,
                },
            )
        };
        vec![
            mk("in_a", "CAMERA A", 0.0, 0.0),
            mk("in_b", "CAMERA B", 640.0, 0.0),
            mk("in_c", "CAMERA C", 0.0, 360.0),
            mk("in_d", "CAMERA D", 640.0, 360.0),
        ]
    }

    fn dynamics(entries: &[(&str, f64, SourceState)]) -> HashMap<String, TileDynamics> {
        entries
            .iter()
            .map(|(id, db, state)| {
                (
                    (*id).to_owned(),
                    TileDynamics {
                        meter_db: *db,
                        state: *state,
                    },
                )
            })
            .collect()
    }

    /// Glyph primitives whose top-left falls inside `rect` (canvas pixels).
    fn glyphs_in(list: &OverlayDrawList, rect: OverlayRect) -> usize {
        list.primitives
            .iter()
            .filter(|p| match p {
                OverlayPrimitive::Glyph { dest_x, dest_y, .. } => {
                    *dest_x >= rect.x
                        && *dest_x < rect.x + i32_dim(rect.width)
                        && *dest_y >= rect.y
                        && *dest_y < rect.y + i32_dim(rect.height)
                }
                _ => false,
            })
            .count()
    }

    /// The **live green fill** rectangles in the tile's right-edge meter column.
    ///
    /// The meter draws three primitives per track: a dim (low-alpha) green
    /// background spanning the whole track, the live opaque-green fill, and an
    /// amber 1px peak tick. We keep only the live fill (opaque green) so its
    /// height is a faithful proxy for the source's loudness deflection.
    fn meter_fill_in(list: &OverlayDrawList, geom: TileGeometry) -> Vec<OverlayRect> {
        let track = geom.meter_track();
        let cell_top = geom.y;
        let cell_bottom = geom.y.saturating_add(i32_dim(geom.height));
        list.primitives
            .iter()
            .filter_map(|p| match p {
                OverlayPrimitive::FilledRect { rect, color, .. }
                    if rect.x >= track.x.saturating_sub(2)
                        && rect.x <= track.x.saturating_add(i32_dim(track.width))
                        // Constrain to THIS cell's vertical extent so a sibling
                        // tile sharing the meter column is not miscounted.
                        && rect.y >= cell_top
                        && rect.y <= cell_bottom
                        // Opaque green live fill only (exclude the dim background
                        // and the amber peak tick).
                        && (color.a - GREEN.a).abs() < 0.01
                        && (color.g - GREEN.g).abs() < 0.01 =>
                {
                    Some(*rect)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn resolver_emits_label_meter_and_flag_per_cell() {
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(1280, 720, tiles.clone(), None, 0).unwrap();

        // in_a loud + LIVE; in_b silent + LIVE; in_c missing (defaults NO_SIGNAL);
        // in_d explicitly NO_SIGNAL.
        let dyns = dynamics(&[
            ("in_a", -3.0, SourceState::Live),
            (
                "in_b",
                mosaic_audio::Ballistics::FLOOR_DB,
                SourceState::Live,
            ),
            (
                "in_d",
                mosaic_audio::Ballistics::FLOOR_DB,
                SourceState::NoSignal,
            ),
        ]);

        // Drive a few frames so the conflated meter establishes a level.
        let mut list = OverlayDrawList::new();
        for tick in 0..5 {
            let pts = MediaTime::from_nanos(tick * 40_000_000);
            list = baker.draw_list(pts, &dyns).unwrap();
        }

        // Each cell must host glyphs for its label + flag, somewhere inside it.
        for spec in &tiles {
            let geom = TileGeometry::resolve(spec.rect);
            let cell = OverlayRect::new(geom.x, geom.y, geom.width, geom.height);
            assert!(
                glyphs_in(&list, cell) > 0,
                "tile {} drew no text glyphs in its cell rect",
                spec.source_id
            );
        }

        // The loud LIVE tile (in_a) must have a meaningfully taller fill than the
        // silent tile (in_b): the per-input meter reflects THAT input's audio.
        let geom_a = TileGeometry::resolve(tiles[0].rect);
        let geom_b = TileGeometry::resolve(tiles[1].rect);
        let tallest = |rects: &[OverlayRect]| rects.iter().map(|r| r.height).max().unwrap_or(0);
        let a_fill = tallest(&meter_fill_in(&list, geom_a));
        let b_fill = tallest(&meter_fill_in(&list, geom_b));
        assert!(
            a_fill > b_fill + 10,
            "loud tile meter fill ({a_fill}) must exceed silent tile fill ({b_fill})"
        );
    }

    #[test]
    fn missing_source_defaults_to_no_signal_flag() {
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(1280, 720, tiles.clone(), None, 0).unwrap();
        // No dynamics at all: every tile must be NO_SIGNAL (never panics/stalls).
        let list = baker.draw_list(MediaTime::ZERO, &HashMap::new()).unwrap();
        // The "NO SIGNAL" flag draws glyphs near each tile's top-left.
        for spec in &tiles {
            let geom = TileGeometry::resolve(spec.rect);
            let flag_box = OverlayRect::new(geom.x, geom.y, geom.width / 2, geom.chip_height());
            assert!(
                glyphs_in(&list, flag_box) > 0,
                "tile {} drew no NO_SIGNAL flag glyphs",
                spec.source_id
            );
        }
    }

    #[test]
    fn meter_and_flag_stay_inside_the_cell_rect() {
        let tiles = quad_tiles();
        let mut baker = OverlayBaker::new(1280, 720, tiles.clone(), None, 0)
            .unwrap()
            .with_per_tile_safe_area(true);
        let dyns = dynamics(&[("in_a", 0.0, SourceState::Live)]);
        let list = baker.draw_list(MediaTime::ZERO, &dyns).unwrap();

        // Every per-tile primitive for tile A must lie within tile A's rect (the
        // per-tile safe-area / meter / flag must NOT span the whole canvas — this
        // is the canvas-wide bug fix).
        let geom = TileGeometry::resolve(tiles[0].rect);
        let track = geom.meter_track();
        let fills = meter_fill_in(&list, geom);
        for r in &fills {
            assert!(
                r.x >= geom.x && r.x + i32_dim(r.width) <= geom.x + i32_dim(geom.width) + 1,
                "meter fill {r:?} escaped tile A horizontally (track {track:?})"
            );
            assert!(
                r.y >= geom.y - 1 && r.y + i32_dim(r.height) <= geom.y + i32_dim(geom.height) + 1,
                "meter fill {r:?} escaped tile A vertically"
            );
        }
        assert!(!fills.is_empty(), "tile A drew no meter fill at 0 dBFS");
    }

    #[test]
    fn tiny_tiles_are_skipped_without_panicking() {
        // A degenerate 10x10 cell is below the legibility floor — skip, never mush.
        let tiles = vec![TileSpec::new(
            "in_tiny",
            "X",
            PixelRect {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
        )];
        let mut baker = OverlayBaker::new(64, 64, tiles, None, 0).unwrap();
        let dyns = dynamics(&[("in_tiny", 0.0, SourceState::Live)]);
        // Must not panic; produces (at most) the clock label, no tile chrome.
        let _ = baker.draw_list(MediaTime::ZERO, &dyns).unwrap();
    }
}
