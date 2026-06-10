//! In-process synthetic video sources (ADR-0027): colour **bars**, a **solid**
//! colour, and a full-frame **clock**.
//!
//! A synthetic source is a source like any other — it produces `Nv12Image`
//! frames into a per-tile `TileStore`, and everything downstream (framestore,
//! compositor, encode-once, output fan-out) treats it identically to a decoded
//! feed. This module is the *renderer* (a pure function of kind + size + wall
//! time) plus the *generator loop* a pipeline runs on a thread that is a peer of
//! a decode thread, publishing at the canvas cadence.
//!
//! `bars` and `solid` are pure pixels and render in every build. The `clock`
//! source composes the existing overlay clock rasterizer onto a solid frame and
//! therefore needs the `overlay` feature; without it, a clock render returns
//! [`SynthError::OverlayRequired`] and the caller falls back honestly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::{ClockFaceConfig, SourceKind};
use multiview_core::time::{MediaTime, Rational};
use multiview_framestore::TileStore;
#[cfg(feature = "overlay")]
use multiview_overlay::clock::TimeZoneOffset;
use multiview_overlay::clock::{ClockFaceMode, Tz, WallTime};

/// An error rendering a synthetic frame.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SynthError {
    /// The compositor rejected the geometry or a colour-pipeline axis.
    #[error("compositor: {0}")]
    Compositor(String),
    /// A `clock` source was asked to render without the `overlay` feature.
    #[error("a clock source needs the `overlay` feature to render")]
    OverlayRequired,
    /// The clock model produced no time for the requested face.
    #[error("clock model produced no time")]
    ClockTime,
}

/// A resolved synthetic source kind (config colour parsed to bytes, clock face
/// flattened), ready to render without re-touching the config.
///
/// Not `Copy`: the `Clock` variant carries an owned `label` string and an IANA
/// zone resolved per render. Clone it (cheap) when handing one to a generator.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SyntheticKind {
    /// 75 % colour bars.
    Bars,
    /// A solid colour.
    Solid {
        /// Red channel (8-bit sRGB-ish).
        r: u8,
        /// Green channel.
        g: u8,
        /// Blue channel.
        b: u8,
    },
    /// A full-frame clock (ADR-0027 / ADR-0047).
    Clock {
        /// The face to draw: analogue, digital, or dual (both).
        mode: ClockFaceMode,
        /// 12-hour vs 24-hour mode (both the dial and the readout).
        twelve_hour: bool,
        /// Resolved IANA timezone, **preferred** when present: the displayed UTC
        /// offset is computed per render (DST-correct). `None` ⇒ the fixed
        /// `tz_offset_minutes` is used.
        tz: Option<Tz>,
        /// Fixed UTC offset in minutes (the legacy / no-DST fallback; ignored
        /// when `tz` is `Some`).
        tz_offset_minutes: i32,
        /// Operator location/label drawn on the face (e.g. `Sydney`).
        label: Option<String>,
        /// Draw a `UTC±HH:MM` offset badge for the displayed instant.
        show_offset: bool,
        /// Draw the disciplined-reference (PTP/NTP/SYS) badge. Display only.
        show_reference: bool,
        /// Draw hour numerals on the analogue / dual face.
        numerals: bool,
    },
}

impl SyntheticKind {
    /// Map a config [`SourceKind`] to a synthetic generator, or `None` for a kind
    /// that needs a decoder (rtsp/hls/ts/srt/rtmp/ndi/file).
    ///
    /// A clock's IANA `timezone` (when set and parseable) wins over the fixed
    /// `tz_offset_minutes`; an unknown id is rejected by config validation before
    /// this point, but is defended here too (an unparseable id falls back to the
    /// fixed offset rather than panicking).
    #[must_use]
    pub fn from_source_kind(kind: &SourceKind) -> Option<Self> {
        match kind {
            SourceKind::Bars => Some(Self::Bars),
            SourceKind::Solid { color } => {
                let (r, g, b) = multiview_config::parse_hex_color(color)?;
                Some(Self::Solid { r, g, b })
            }
            SourceKind::Clock {
                face,
                twelve_hour,
                timezone,
                tz_offset_minutes,
                label,
                show_offset,
                show_reference,
                numerals,
            } => Some(Self::Clock {
                mode: face_mode(*face),
                twelve_hour: *twelve_hour,
                tz: timezone
                    .as_deref()
                    .and_then(multiview_overlay::clock::parse_tz),
                tz_offset_minutes: *tz_offset_minutes,
                label: label.clone(),
                show_offset: *show_offset,
                show_reference: *show_reference,
                numerals: *numerals,
            }),
            _ => None,
        }
    }

    /// Whether this source's picture changes over time (so a generator must
    /// re-render, not just republish a cached frame).
    #[must_use]
    pub const fn animated(&self) -> bool {
        matches!(self, Self::Clock { .. })
    }
}

/// Map the config [`ClockFaceConfig`] to the overlay [`ClockFaceMode`].
fn face_mode(face: ClockFaceConfig) -> ClockFaceMode {
    match face {
        ClockFaceConfig::Digital => ClockFaceMode::Digital,
        ClockFaceConfig::Dual => ClockFaceMode::Dual,
        // `Analog` plus, since `ClockFaceConfig` is `#[non_exhaustive]`, any
        // future face defaults to the analogue dial rather than failing the
        // render.
        ClockFaceConfig::Analog | _ => ClockFaceMode::Analog,
    }
}

/// Render one frame for displayed wall time `now`. `bars`/`solid` ignore `now`.
///
/// # Errors
///
/// [`SynthError::Compositor`] on a geometry/colour failure, or
/// [`SynthError::OverlayRequired`] for a `clock` source in a build without the
/// `overlay` feature.
// `now` is the displayed instant; only the `overlay`-gated clock arm reads it
// (bars/solid ignore it, and without `overlay` the clock arm short-circuits), so
// it is unused in the default build — the parameter stays part of the public
// signature regardless of feature set.
#[cfg_attr(not(feature = "overlay"), allow(unused_variables))]
pub fn render(
    kind: &SyntheticKind,
    width: u32,
    height: u32,
    canvas: CanvasColor,
    now: WallTime,
) -> Result<Nv12Image, SynthError> {
    match kind {
        SyntheticKind::Bars => Nv12Image::color_bars(width, height, canvas)
            .map_err(|e| SynthError::Compositor(e.to_string())),
        SyntheticKind::Solid { r, g, b } => Nv12Image::solid_rgb(width, height, *r, *g, *b, canvas)
            .map_err(|e| SynthError::Compositor(e.to_string())),
        #[cfg(feature = "overlay")]
        SyntheticKind::Clock {
            mode,
            twelve_hour,
            tz,
            tz_offset_minutes,
            label,
            show_offset,
            show_reference,
            numerals,
        } => {
            // Resolve the displayed offset for THIS instant: an IANA zone is
            // DST-correct per render; otherwise the fixed offset. (Integer/chrono
            // only — never float time.)
            let offset = match tz {
                Some(tz) => multiview_overlay::clock::resolve_offset(*tz, now),
                None => TimeZoneOffset::from_minutes(*tz_offset_minutes),
            };
            render_clock(
                width,
                height,
                canvas,
                ClockRender {
                    mode: *mode,
                    twelve_hour: *twelve_hour,
                    offset,
                    label: label.as_deref(),
                    show_offset: *show_offset,
                    show_reference: *show_reference,
                    numerals: *numerals,
                },
                now,
            )
        }
        // Without the `overlay` feature a clock cannot be baked; the caller falls
        // back honestly (ADR-0027) rather than panicking.
        #[cfg(not(feature = "overlay"))]
        SyntheticKind::Clock { .. } => Err(SynthError::OverlayRequired),
    }
}

/// The resolved-per-instant clock render parameters handed to `render_clock`.
#[cfg(feature = "overlay")]
#[allow(clippy::struct_excessive_bools)]
// reason: these are four INDEPENDENT display toggles (offset badge / reference
// badge / hour numerals / 12-vs-24h), not a state that should be an enum — they
// mirror the config flags one-to-one. A bag of named flags is the clearest shape.
#[derive(Debug, Clone, Copy)]
struct ClockRender<'a> {
    /// The face: analogue, digital, or dual.
    mode: ClockFaceMode,
    /// 12-hour vs 24-hour mode.
    twelve_hour: bool,
    /// The UTC offset resolved for the displayed instant.
    offset: TimeZoneOffset,
    /// Operator location/label (drawn in the metadata strip).
    label: Option<&'a str>,
    /// Draw the `UTC±HH:MM` offset badge.
    show_offset: bool,
    /// Draw the disciplined-reference badge (display only).
    show_reference: bool,
    /// Draw hour numerals on the analogue / dual face.
    numerals: bool,
}

/// The key that decides when a generator must re-render: the displayed second
/// for a clock (the picture only changes once a second), a constant otherwise.
#[must_use]
fn render_key(kind: &SyntheticKind, now: WallTime) -> i64 {
    if kind.animated() {
        now.unix_seconds()
    } else {
        0
    }
}

#[cfg(feature = "overlay")]
fn render_clock(
    width: u32,
    height: u32,
    canvas: CanvasColor,
    spec: ClockRender<'_>,
    now: WallTime,
) -> Result<Nv12Image, SynthError> {
    use multiview_compositor::overlay::subpass::{apply_overlays_to_nv12, OverlayDrawList};
    use multiview_compositor::overlay::text::TextEngine;

    let cc = |e: multiview_compositor::Error| SynthError::Compositor(e.to_string());
    // A near-black slate to draw the clock onto.
    let bg = Nv12Image::solid_rgb(width, height, 8, 8, 12, canvas).map_err(cc)?;
    let mut engine = TextEngine::new().map_err(cc)?;
    let mut list = OverlayDrawList::new();

    // Reserve the bottom strip for the metadata (label + offset/reference badges)
    // when any is requested; the face(s) share the rest. All sizing is integer-
    // derived from the tile dimensions (no magic px).
    let has_strip = spec.label.is_some() || spec.show_offset || spec.show_reference;
    let strip_h = if has_strip {
        (height.saturating_mul(18) / 100).max(1)
    } else {
        0
    };
    let face_h = height.saturating_sub(strip_h).max(1);

    // In dual mode the readout takes the lower ~28 % of the face region and the
    // analogue dial sits in the upper region above it (brief §2.2 — "an analogue
    // face with a digital readout beneath it"). Either face alone fills `face_h`.
    let dual = spec.mode.has_analog() && spec.mode.has_digital();
    let readout_h = if dual {
        (face_h.saturating_mul(28) / 100).max(1)
    } else {
        face_h
    };
    let dial_h = if dual {
        face_h.saturating_sub(readout_h).max(1)
    } else {
        face_h
    };

    if spec.mode.has_analog() {
        draw_analog_face(&mut list, &mut engine, width, dial_h, spec, now)?;
    }
    if spec.mode.has_digital() {
        // Digital-only fills the face region; dual places the readout in the lower
        // band, beneath the dial.
        let top = if dual { dial_h } else { 0 };
        draw_digital_readout(&mut list, &mut engine, width, top, readout_h, spec, now)?;
    }
    if has_strip {
        draw_metadata_strip(
            &mut list,
            &mut engine,
            width,
            height.saturating_sub(strip_h),
            strip_h,
            spec,
        )?;
    }

    apply_overlays_to_nv12(&bg, &list, canvas).map_err(cc)
}

/// Draw the analogue dial (bezel, ticks, hands, hub) centred in the top `face_h`
/// pixels of a `width`-wide tile, plus optional hour numerals.
#[cfg(feature = "overlay")]
fn draw_analog_face(
    list: &mut multiview_compositor::overlay::subpass::OverlayDrawList,
    engine: &mut multiview_compositor::overlay::text::TextEngine,
    width: u32,
    face_h: u32,
    spec: ClockRender<'_>,
    now: WallTime,
) -> Result<(), SynthError> {
    use multiview_compositor::overlay::subpass::{
        clock_face, ClockFaceStyle, HandAngles, OverlayColor,
    };
    use multiview_overlay::clock::AnalogHands;

    let local = now.with_offset(spec.offset);
    let hands = AnalogHands::for_dial(local, spec.twelve_hour);
    let hour_ticks = if spec.twelve_hour { 12 } else { 24 };
    // Bezel ≈ 45 % of the smaller of (width, face_h), centred in the face region.
    let radius = width.min(face_h).saturating_mul(9) / 20;
    let cx = f32_of(width) / 2.0;
    let cy = f32_of(face_h) / 2.0;
    let style = ClockFaceStyle::at(cx, cy, f32_of(radius));
    for prim in clock_face(
        HandAngles {
            hour_deg: hands.hour_deg,
            minute_deg: hands.minute_deg,
            second_deg: hands.second_deg,
        },
        style,
        hour_ticks,
    ) {
        list.push(prim);
    }

    if spec.numerals {
        // Hour numerals at each tick position, just inside the bezel. Reuses the
        // same unit-vector placement the ticks use; one short text run per hour.
        let cc = |e: multiview_compositor::Error| SynthError::Compositor(e.to_string());
        let numeral_r = f32_of(radius) * 0.74;
        let size_px = (f32_of(radius) * 0.20).max(6.0);
        let color = OverlayColor::opaque(0.95, 0.95, 0.95);
        for hour in 1..=hour_ticks {
            // 12/24 sits at the top (0°); subsequent numerals advance clockwise.
            let deg = (f32_of(hour % hour_ticks)) * (360.0 / f32_of(hour_ticks));
            let (ux, uy) = unit_vector_deg(deg);
            let nx = cx + ux * numeral_r;
            let ny = cy + uy * numeral_r;
            let run = engine
                .rasterize_run(
                    &hour.to_string(),
                    multiview_compositor::overlay::text::FontFamily::Sans,
                    size_px,
                    [0.95, 0.95, 0.95, 1.0],
                )
                .map_err(cc)?;
            push_run_centred_at(list, &run, round_to_i32(nx), round_to_i32(ny), color);
        }
    }
    Ok(())
}

/// Draw the digital `HH:MM:SS` readout, centred in the region `[top, top+region_h)`.
#[cfg(feature = "overlay")]
fn draw_digital_readout(
    list: &mut multiview_compositor::overlay::subpass::OverlayDrawList,
    engine: &mut multiview_compositor::overlay::text::TextEngine,
    width: u32,
    top: u32,
    region_h: u32,
    spec: ClockRender<'_>,
    now: WallTime,
) -> Result<(), SynthError> {
    use multiview_compositor::overlay::subpass::OverlayColor;
    use multiview_compositor::overlay::text::FontFamily;
    use multiview_overlay::clock::{ClockFace, ClockModel, RefSource, RefStatus, TimeRef};

    let cc = |e: multiview_compositor::Error| SynthError::Compositor(e.to_string());
    // The time reference is the host system clock (free-running). The value only
    // feeds the optional a11y badge in the metadata strip (display only).
    let time_ref = TimeRef::new(RefSource::System, RefStatus::Freerun);
    let face = if spec.twelve_hour {
        ClockFace::digital_12h()
    } else {
        ClockFace::digital_24h()
    };
    let model = ClockModel::new(face, spec.offset, time_ref);
    let text = model.render_digital(now).ok_or(SynthError::ClockTime)?;
    // Size the readout to a fraction of the region height.
    let size_px = (f32_of(region_h) / 2.5).max(8.0);
    let color = OverlayColor::opaque(0.95, 0.95, 0.95);
    let run = engine
        .rasterize_run(&text, FontFamily::Mono, size_px, [0.95, 0.95, 0.95, 1.0])
        .map_err(cc)?;
    let cx = round_to_i32(f32_of(width) / 2.0);
    let cy = round_to_i32(f32_of(top) + f32_of(region_h) / 2.0);
    push_run_centred_at(list, &run, cx, cy, color);
    Ok(())
}

/// Draw the metadata strip in `[top, top+strip_h)`: the label (left), the
/// `UTC±HH:MM` offset badge (right), and the disciplined-reference badge.
#[cfg(feature = "overlay")]
fn draw_metadata_strip(
    list: &mut multiview_compositor::overlay::subpass::OverlayDrawList,
    engine: &mut multiview_compositor::overlay::text::TextEngine,
    width: u32,
    top: u32,
    strip_h: u32,
    spec: ClockRender<'_>,
) -> Result<(), SynthError> {
    use multiview_compositor::overlay::subpass::OverlayColor;
    use multiview_compositor::overlay::text::FontFamily;
    use multiview_overlay::clock::{RefSource, RefStatus, TimeRef};

    let cc = |e: multiview_compositor::Error| SynthError::Compositor(e.to_string());
    let size_px = (f32_of(strip_h) * 0.62).max(8.0);
    let color = OverlayColor::opaque(0.90, 0.90, 0.90);
    let pad = i32_of(width.saturating_mul(3) / 100).max(2);
    let baseline_y = round_to_i32(f32_of(top) + f32_of(strip_h) / 2.0);

    // Label — left-aligned in the strip.
    if let Some(label) = spec.label {
        if !label.is_empty() {
            let run = engine
                .rasterize_run(label, FontFamily::Sans, size_px, [0.90, 0.90, 0.90, 1.0])
                .map_err(cc)?;
            push_run_left_at(list, &run, pad, baseline_y, color);
        }
    }

    // UTC-offset badge — right-aligned in the strip.
    if spec.show_offset {
        let badge = spec.offset.utc_badge();
        let run = engine
            .rasterize_run(&badge, FontFamily::Mono, size_px, [0.90, 0.90, 0.90, 1.0])
            .map_err(cc)?;
        let run_w = run_width(&run);
        let right_x = i32_of(width).saturating_sub(pad).saturating_sub(run_w);
        push_run_left_at(list, &run, right_x, baseline_y, color);
    }

    // Disciplined-reference badge — centred (display only; never paces, ADR-T012).
    if spec.show_reference {
        let time_ref = TimeRef::new(RefSource::System, RefStatus::Freerun);
        let run = engine
            .rasterize_run(
                &time_ref.status_text(),
                FontFamily::Sans,
                size_px,
                [0.90, 0.90, 0.90, 1.0],
            )
            .map_err(cc)?;
        push_run_centred_at(list, &run, i32_of(width) / 2, baseline_y, color);
    }
    Ok(())
}

/// `u32 -> f32` without an `as` cast (dimensions fit u16 well within 8K).
#[cfg(feature = "overlay")]
#[must_use]
fn f32_of(v: u32) -> f32 {
    f32::from(u16::try_from(v).unwrap_or(u16::MAX))
}

/// `u32 -> i32` saturating (no `as` cast).
#[cfg(feature = "overlay")]
#[must_use]
fn i32_of(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// Round an `f32` to the nearest `i32` (saturating), no `as` cast. The magnitude
/// is found by a binary search over `u32` (comparing against `u32_to_f32`), then
/// signed — there is no float-to-int cast anywhere on the path.
#[cfg(feature = "overlay")]
#[must_use]
fn round_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }
    let target = value.abs().round();
    let mut lo = 0_u32;
    let mut hi = i32::MAX.unsigned_abs();
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if u32_to_f32(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    let n = i32::try_from(lo).unwrap_or(i32::MAX);
    if value < 0.0 {
        n.saturating_neg()
    } else {
        n
    }
}

/// `u32 -> f32` (lossy beyond 2^24, but clock geometry is far below that), no
/// `as` cast: split into hi/lo 16-bit halves and combine.
#[cfg(feature = "overlay")]
#[must_use]
fn u32_to_f32(v: u32) -> f32 {
    let hi = f32::from(u16::try_from(v >> 16).unwrap_or(u16::MAX));
    let lo = f32::from(u16::try_from(v & 0xFFFF).unwrap_or(u16::MAX));
    hi * 65_536.0 + lo
}

/// The unit direction vector for `deg` degrees **clockwise from 12 o'clock**
/// (straight up). Screen y is downward, so up is `-y`: `(sin θ, -cos θ)` — the
/// same convention the compositor's clock-face ticks use.
#[cfg(feature = "overlay")]
#[must_use]
fn unit_vector_deg(deg: f32) -> (f32, f32) {
    let rad = deg.to_radians();
    (rad.sin(), -rad.cos())
}

/// The tight bounding box of a rasterized run, in run-local pixels, or `None`
/// when the run has no drawable glyphs.
#[cfg(feature = "overlay")]
#[must_use]
fn run_bbox(
    run: &multiview_compositor::overlay::text::RasterizedRun,
) -> Option<(i32, i32, i32, i32)> {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for g in run.glyphs() {
        min_x = min_x.min(g.dest_x);
        min_y = min_y.min(g.dest_y);
        max_x = max_x.max(g.dest_x.saturating_add(i32_of(g.width)));
        max_y = max_y.max(g.dest_y.saturating_add(i32_of(g.height)));
    }
    (max_x >= min_x && max_y >= min_y).then_some((min_x, min_y, max_x, max_y))
}

/// The drawable width of a rasterized run, in pixels (`0` for an empty run).
#[cfg(feature = "overlay")]
#[must_use]
fn run_width(run: &multiview_compositor::overlay::text::RasterizedRun) -> i32 {
    run_bbox(run).map_or(0, |(min_x, _, max_x, _)| max_x.saturating_sub(min_x))
}

/// Push every glyph of `run` into `list`, translated by `(off_x, off_y)` and
/// tinted `color`. Only the straight coverage (alpha) is carried; the linear
/// `color` supplies the tint (overlay-rendering.md §4.1).
#[cfg(feature = "overlay")]
fn push_run_offset(
    list: &mut multiview_compositor::overlay::subpass::OverlayDrawList,
    run: &multiview_compositor::overlay::text::RasterizedRun,
    off_x: i32,
    off_y: i32,
    color: multiview_compositor::overlay::subpass::OverlayColor,
) {
    use multiview_compositor::overlay::subpass::OverlayPrimitive;
    for g in run.glyphs() {
        list.push(OverlayPrimitive::Glyph {
            dest_x: g.dest_x.saturating_add(off_x),
            dest_y: g.dest_y.saturating_add(off_y),
            width: g.width,
            height: g.height,
            coverage: g
                .premultiplied_rgba
                .chunks_exact(4)
                .filter_map(|px| px.get(3).copied())
                .collect(),
            color,
        });
    }
}

/// Push `run` centred (both axes) at canvas `(cx, cy)`.
#[cfg(feature = "overlay")]
fn push_run_centred_at(
    list: &mut multiview_compositor::overlay::subpass::OverlayDrawList,
    run: &multiview_compositor::overlay::text::RasterizedRun,
    cx: i32,
    cy: i32,
    color: multiview_compositor::overlay::subpass::OverlayColor,
) {
    if let Some((min_x, min_y, max_x, max_y)) = run_bbox(run) {
        let run_w = max_x.saturating_sub(min_x);
        let run_h = max_y.saturating_sub(min_y);
        let off_x = cx.saturating_sub(run_w / 2).saturating_sub(min_x);
        let off_y = cy.saturating_sub(run_h / 2).saturating_sub(min_y);
        push_run_offset(list, run, off_x, off_y, color);
    }
}

/// Push `run` with its left edge at `left_x` and vertically centred at `cy`.
#[cfg(feature = "overlay")]
fn push_run_left_at(
    list: &mut multiview_compositor::overlay::subpass::OverlayDrawList,
    run: &multiview_compositor::overlay::text::RasterizedRun,
    left_x: i32,
    cy: i32,
    color: multiview_compositor::overlay::subpass::OverlayColor,
) {
    if let Some((min_x, min_y, _, max_y)) = run_bbox(run) {
        let run_h = max_y.saturating_sub(min_y);
        let off_x = left_x.saturating_sub(min_x);
        let off_y = cy.saturating_sub(run_h / 2).saturating_sub(min_y);
        push_run_offset(list, run, off_x, off_y, color);
    }
}

/// The host wall clock as whole UNIX seconds (the clock face's resolution is one
/// second). Independent of the `overlay`-gated `wallclock` module so the
/// generator compiles in every build.
#[must_use]
fn unix_now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

/// The wall-clock duration of one output tick at `cadence` (`den/num` seconds).
#[must_use]
fn tick_interval(cadence: Rational) -> Duration {
    let num = u64::try_from(cadence.num).unwrap_or(1).max(1);
    let den = u64::try_from(cadence.den).unwrap_or(1).max(1);
    // den/num seconds in nanos = den * 1e9 / num.
    let nanos = den.saturating_mul(1_000_000_000) / num;
    Duration::from_nanos(nanos.clamp(1, 1_000_000_000))
}

/// Run a synthetic source: render and publish a frame into `store` every tick at
/// `cadence` until `stop` is raised. A clock re-renders only when its displayed
/// second changes (otherwise the cached frame is republished, just re-stamped),
/// so an animated clock costs one bake per second, not one per tick.
///
/// This is a peer of a decode thread (the caller spawns it on a thread and joins
/// on `stop`): it never blocks the engine, and a render failure holds the last
/// good frame rather than stalling.
#[allow(clippy::needless_pass_by_value)]
// reason: the generator OWNS its `kind` for the whole thread lifetime (the caller
// moves a clone in); it is read by reference each tick but never returned, so
// taking it by value is the correct ownership, not a needless copy.
pub fn generator_loop(
    kind: SyntheticKind,
    store: &TileStore<Nv12Image>,
    width: u32,
    height: u32,
    canvas: CanvasColor,
    cadence: Rational,
    stop: &AtomicBool,
) {
    let interval = tick_interval(cadence);
    let start = Instant::now();
    // (render-key, frame) so a clock re-renders once a second and statics once.
    let mut cached: Option<(i64, Arc<Nv12Image>)> = None;

    while !stop.load(Ordering::Acquire) {
        let now = WallTime::from_unix_seconds(unix_now_seconds());
        let key = render_key(&kind, now);
        let frame = match &cached {
            Some((k, f)) if *k == key => Arc::clone(f),
            _ => match render(&kind, width, height, canvas, now) {
                Ok(image) => {
                    let f = Arc::new(image);
                    cached = Some((key, Arc::clone(&f)));
                    f
                }
                Err(e) => {
                    tracing::warn!(error = %e, "synthetic source render failed; holding last good");
                    sleep_until(interval, stop);
                    continue;
                }
            },
        };
        let elapsed = Instant::now().saturating_duration_since(start);
        let at = MediaTime::from_nanos(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX));
        store.publish_arc(frame, at);
        sleep_until(interval, stop);
    }
}

/// Sleep for `interval`, waking every ≤25 ms to re-check `stop` so teardown is
/// prompt (a wedged synthetic source can never delay a join past one chunk).
fn sleep_until(interval: Duration, stop: &AtomicBool) {
    const CHUNK: Duration = Duration::from_millis(25);
    let deadline = Instant::now() + interval;
    while Instant::now() < deadline {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(CHUNK));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiview_config::MultiviewConfig;

    fn canvas() -> CanvasColor {
        CanvasColor::default()
    }

    fn kind_of(fields: &str) -> SyntheticKind {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 320
height = 240
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"
[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]
[[sources]]
id = "in_a"
{fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse");
        let src = cfg.sources.into_iter().next().expect("source");
        SyntheticKind::from_source_kind(&src.kind).expect("synthetic kind")
    }

    #[test]
    fn from_source_kind_maps_synthetics_and_skips_decoded() {
        assert_eq!(kind_of("kind = \"bars\""), SyntheticKind::Bars);
        assert_eq!(
            kind_of("kind = \"solid\"\ncolor = \"#22aa44\""),
            SyntheticKind::Solid {
                r: 0x22,
                g: 0xaa,
                b: 0x44
            }
        );
        assert!(matches!(
            kind_of("kind = \"clock\""),
            SyntheticKind::Clock {
                mode: ClockFaceMode::Analog,
                ..
            }
        ));
        // The dual face resolves to the 3-state mode and carries the IANA zone +
        // metadata; the fixed-offset fallback is used only when no zone is set.
        assert!(matches!(
            kind_of(
                "kind = \"clock\"\nface = \"dual\"\ntimezone = \"Australia/Sydney\"\nlabel = \"Sydney\"\nshow_offset = true"
            ),
            SyntheticKind::Clock {
                mode: ClockFaceMode::Dual,
                tz: Some(_),
                show_offset: true,
                ..
            }
        ));
        // A decoded kind is not synthetic.
        let doc = "schema_version=1\n[canvas]\nwidth=64\nheight=64\nfps=\"25/1\"\npixel_format=\"nv12\"\nbackground=\"#101014\"\n[canvas.color]\nprofile=\"sdr-bt709-limited\"\n[layout]\nkind=\"grid\"\ncolumns=[\"1fr\"]\nrows=[\"1fr\"]\nareas=[\"a\"]\n[[sources]]\nid=\"s\"\nkind=\"rtsp\"\nurl=\"rtsp://x/y\"\n[[cells]]\nid=\"c\"\narea=\"a\"\n[cells.source]\ninput_id=\"s\"\n[[outputs]]\nkind=\"hls\"\npath=\"/tmp/x.m3u8\"\ncodec=\"mpeg2video\"\nsegment_ms=1000\n";
        let cfg = MultiviewConfig::load_from_toml(doc).expect("parse");
        assert!(SyntheticKind::from_source_kind(&cfg.sources[0].kind).is_none());
    }

    #[test]
    fn bars_render_has_the_descending_luma_staircase() {
        let img = render(
            &SyntheticKind::Bars,
            560,
            240,
            canvas(),
            WallTime::from_unix_seconds(0),
        )
        .expect("bars");
        let lumas: Vec<u8> = (0..7_u32)
            .map(|k| img.sample((k * 560 / 7) + 40, 120).expect("sample").0)
            .collect();
        for w in lumas.windows(2) {
            assert!(w[0] > w[1], "bars staircase: {lumas:?}");
        }
    }

    #[test]
    fn solid_render_is_uniform() {
        let img = render(
            &SyntheticKind::Solid {
                r: 0x22,
                g: 0xaa,
                b: 0x44,
            },
            64,
            64,
            canvas(),
            WallTime::from_unix_seconds(0),
        )
        .expect("solid");
        let tl = img.sample(0, 0).expect("tl");
        assert_eq!(img.sample(63, 63).expect("br"), tl);
    }

    /// A bare clock descriptor (fixed offset, no metadata) at `mode`/`twelve_hour`.
    /// Only the `overlay`-gated render tests use it.
    #[cfg(feature = "overlay")]
    fn clock_kind(mode: ClockFaceMode, twelve_hour: bool, tz_offset_minutes: i32) -> SyntheticKind {
        SyntheticKind::Clock {
            mode,
            twelve_hour,
            tz: None,
            tz_offset_minutes,
            label: None,
            show_offset: false,
            show_reference: false,
            numerals: false,
        }
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn analog_clock_renders_and_animates() {
        let c = canvas();
        // 03:00:00 UTC and 09:00:00 UTC must differ (the hands moved).
        let three = render(
            &clock_kind(ClockFaceMode::Analog, false, 0),
            240,
            240,
            c,
            WallTime::from_unix_seconds(3 * 3600),
        )
        .expect("clock 3:00");
        let nine = render(
            &clock_kind(ClockFaceMode::Analog, false, 0),
            240,
            240,
            c,
            WallTime::from_unix_seconds(9 * 3600),
        )
        .expect("clock 9:00");
        let bg = Nv12Image::solid_rgb(240, 240, 8, 8, 12, c).expect("bg");
        // The clock drew something (differs from the blank slate) and it animates
        // (3:00 differs from 9:00) — content-aware, not a byte hash of nothing.
        assert_ne!(
            three.y_plane(),
            bg.y_plane(),
            "clock drew a face onto the slate"
        );
        assert_ne!(
            three.y_plane(),
            nine.y_plane(),
            "the clock animates with time"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn analog_clock_honors_12h_vs_24h_dial() {
        let c = canvas();
        // 18:00 UTC: a 12-hour dial puts the hour hand at 6 o'clock (180°, down);
        // a 24-hour dial (15°/hour, 24 ticks) puts it at 270° (left). The two
        // dials must render a visibly different face.
        let render_dial = |twelve_hour| {
            render(
                &clock_kind(ClockFaceMode::Analog, twelve_hour, 0),
                240,
                240,
                c,
                WallTime::from_unix_seconds(18 * 3600),
            )
            .expect("analog clock")
        };
        assert_ne!(
            render_dial(true).y_plane(),
            render_dial(false).y_plane(),
            "12-hour and 24-hour analog dials must render differently"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn digital_clock_renders_onto_the_slate() {
        let c = canvas();
        let img = render(
            &clock_kind(ClockFaceMode::Digital, false, 0),
            320,
            120,
            c,
            WallTime::from_unix_seconds(12 * 3600 + 34 * 60 + 56),
        )
        .expect("digital clock");
        let bg = Nv12Image::solid_rgb(320, 120, 8, 8, 12, c).expect("bg");
        assert_ne!(
            img.y_plane(),
            bg.y_plane(),
            "the digital readout drew glyphs onto the slate"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn dual_clock_differs_from_analog_and_digital_at_the_same_instant() {
        let c = canvas();
        // A fixed instant (12:34:56 UTC); inject it directly so the test never
        // reads the system clock (deterministic — the clock abstraction is the
        // injected `WallTime`).
        let at = WallTime::from_unix_seconds(12 * 3600 + 34 * 60 + 56);
        let dim = (320, 320);
        let dual = render(
            &clock_kind(ClockFaceMode::Dual, false, 0),
            dim.0,
            dim.1,
            c,
            at,
        )
        .expect("dual clock");
        let analog = render(
            &clock_kind(ClockFaceMode::Analog, false, 0),
            dim.0,
            dim.1,
            c,
            at,
        )
        .expect("analog clock");
        let digital = render(
            &clock_kind(ClockFaceMode::Digital, false, 0),
            dim.0,
            dim.1,
            c,
            at,
        )
        .expect("digital clock");
        let bg = Nv12Image::solid_rgb(dim.0, dim.1, 8, 8, 12, c).expect("bg");
        // Dual drew something, and it is neither the pure analog face nor the pure
        // digital readout (it placed BOTH — the face up top, the readout below).
        assert_ne!(dual.y_plane(), bg.y_plane(), "dual drew onto the slate");
        assert_ne!(
            dual.y_plane(),
            analog.y_plane(),
            "dual is not just the analog face (it added the digital readout)"
        );
        assert_ne!(
            dual.y_plane(),
            digital.y_plane(),
            "dual is not just the digital readout (it added the analog face)"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn dual_clock_animates_second_by_second() {
        let c = canvas();
        let base = 12 * 3600 + 34 * 60;
        let a = render(
            &clock_kind(ClockFaceMode::Dual, false, 0),
            320,
            320,
            c,
            WallTime::from_unix_seconds(base + 4),
        )
        .expect("dual :04");
        let b = render(
            &clock_kind(ClockFaceMode::Dual, false, 0),
            320,
            320,
            c,
            WallTime::from_unix_seconds(base + 5),
        )
        .expect("dual :05");
        assert_ne!(
            a.y_plane(),
            b.y_plane(),
            "the dual clock advances with the displayed second"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn labeled_clock_differs_from_unlabeled() {
        let c = canvas();
        let at = WallTime::from_unix_seconds(9 * 3600);
        let labeled = render(
            &SyntheticKind::Clock {
                mode: ClockFaceMode::Dual,
                twelve_hour: false,
                tz: None,
                tz_offset_minutes: 0,
                label: Some("Sydney".to_owned()),
                show_offset: false,
                show_reference: false,
                numerals: false,
            },
            320,
            320,
            c,
            at,
        )
        .expect("labeled");
        let plain =
            render(&clock_kind(ClockFaceMode::Dual, false, 0), 320, 320, c, at).expect("plain");
        assert_ne!(
            labeled.y_plane(),
            plain.y_plane(),
            "the label line drew glyphs that the plain clock did not"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn show_offset_badge_differs_across_a_dst_boundary_for_one_zone() {
        // Same Sydney clock, two instants either side of the austral DST change:
        // the resolved offset (UTC+11:00 vs UTC+10:00) changes the badge text, so
        // the rendered strip must differ. Inject both instants (deterministic).
        let c = canvas();
        let syd = multiview_overlay::clock::parse_tz("Australia/Sydney").expect("zone");
        let make = |unix: i64| {
            render(
                &SyntheticKind::Clock {
                    mode: ClockFaceMode::Dual,
                    twelve_hour: false,
                    tz: Some(syd),
                    tz_offset_minutes: 0,
                    label: None,
                    show_offset: true,
                    show_reference: false,
                    numerals: false,
                },
                320,
                320,
                c,
                WallTime::from_unix_seconds(unix),
            )
            .expect("sydney clock")
        };
        // 2026-01-15 00:00 UTC (DST, +11) vs 2026-07-15 00:00 UTC (standard, +10).
        let jan = make(1_768_435_200);
        let jul = make(1_784_073_600);
        assert_ne!(
            jan.y_plane(),
            jul.y_plane(),
            "the UTC-offset badge follows DST (UTC+11:00 in Jan vs UTC+10:00 in Jul)"
        );
    }

    #[cfg(feature = "overlay")]
    #[test]
    fn numerals_add_glyphs_to_the_analog_face() {
        let c = canvas();
        let at = WallTime::from_unix_seconds(10 * 3600 + 8 * 60);
        let with = render(
            &SyntheticKind::Clock {
                mode: ClockFaceMode::Analog,
                twelve_hour: true,
                tz: None,
                tz_offset_minutes: 0,
                label: None,
                show_offset: false,
                show_reference: false,
                numerals: true,
            },
            320,
            320,
            c,
            at,
        )
        .expect("with numerals");
        let without = render(&clock_kind(ClockFaceMode::Analog, true, 0), 320, 320, c, at)
            .expect("no numerals");
        assert_ne!(
            with.y_plane(),
            without.y_plane(),
            "hour numerals drew glyphs at the tick positions"
        );
    }
}
