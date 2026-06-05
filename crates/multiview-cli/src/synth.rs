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
use multiview_overlay::clock::WallTime;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A full-frame clock.
    Clock {
        /// `true` for an analog face, `false` for a digital readout.
        analog: bool,
        /// 12-hour digital readout (ignored for analog).
        twelve_hour: bool,
        /// Timezone offset from UTC in minutes.
        tz_offset_minutes: i32,
    },
}

impl SyntheticKind {
    /// Map a config [`SourceKind`] to a synthetic generator, or `None` for a kind
    /// that needs a decoder (rtsp/hls/ts/srt/rtmp/ndi/file).
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
                tz_offset_minutes,
            } => Some(Self::Clock {
                analog: matches!(face, ClockFaceConfig::Analog),
                twelve_hour: *twelve_hour,
                tz_offset_minutes: *tz_offset_minutes,
            }),
            _ => None,
        }
    }

    /// Whether this source's picture changes over time (so a generator must
    /// re-render, not just republish a cached frame).
    #[must_use]
    pub const fn animated(self) -> bool {
        matches!(self, Self::Clock { .. })
    }
}

/// Render one frame for displayed wall time `now`. `bars`/`solid` ignore `now`.
///
/// # Errors
///
/// [`SynthError::Compositor`] on a geometry/colour failure, or
/// [`SynthError::OverlayRequired`] for a `clock` source in a build without the
/// `overlay` feature.
pub fn render(
    kind: SyntheticKind,
    width: u32,
    height: u32,
    canvas: CanvasColor,
    now: WallTime,
) -> Result<Nv12Image, SynthError> {
    match kind {
        SyntheticKind::Bars => Nv12Image::color_bars(width, height, canvas)
            .map_err(|e| SynthError::Compositor(e.to_string())),
        SyntheticKind::Solid { r, g, b } => Nv12Image::solid_rgb(width, height, r, g, b, canvas)
            .map_err(|e| SynthError::Compositor(e.to_string())),
        SyntheticKind::Clock {
            analog,
            twelve_hour,
            tz_offset_minutes,
        } => render_clock(
            width,
            height,
            canvas,
            analog,
            twelve_hour,
            tz_offset_minutes,
            now,
        ),
    }
}

/// The key that decides when a generator must re-render: the displayed second
/// for a clock (the picture only changes once a second), a constant otherwise.
#[must_use]
fn render_key(kind: SyntheticKind, now: WallTime) -> i64 {
    if kind.animated() {
        now.unix_seconds()
    } else {
        0
    }
}

#[cfg(feature = "overlay")]
#[allow(clippy::too_many_arguments)]
// reason: a flat render entry; the clock parameters (analog/12h/tz) are distinct
// scalars and bundling them into a struct would not improve clarity here.
fn render_clock(
    width: u32,
    height: u32,
    canvas: CanvasColor,
    analog: bool,
    twelve_hour: bool,
    tz_offset_minutes: i32,
    now: WallTime,
) -> Result<Nv12Image, SynthError> {
    use multiview_compositor::overlay::subpass::{
        apply_overlays_to_nv12, clock_face, ClockFaceStyle, HandAngles, OverlayColor,
        OverlayDrawList, OverlayPrimitive,
    };
    use multiview_compositor::overlay::text::{FontFamily, TextEngine};
    use multiview_overlay::clock::{
        AnalogHands, ClockFace, ClockModel, RefSource, RefStatus, TimeRef, TimeZoneOffset,
    };

    let cc = |e: multiview_compositor::Error| SynthError::Compositor(e.to_string());
    // A near-black slate to draw the clock onto.
    let bg = Nv12Image::solid_rgb(width, height, 8, 8, 12, canvas).map_err(cc)?;
    let zone = TimeZoneOffset::from_minutes(tz_offset_minutes);
    let mut list = OverlayDrawList::new();

    if analog {
        // `twelve_hour` selects the dial: a 12-hour dial (12 ticks, hour hand two
        // revolutions/day) or a 24-hour dial (24 ticks, one revolution/day).
        let hands = AnalogHands::for_dial(now.with_offset(zone), twelve_hour);
        let hour_ticks = if twelve_hour { 12 } else { 24 };
        // Bezel ≈ 45 % of the smaller dimension, centred.
        let radius = width.min(height).saturating_mul(9) / 20;
        let style = ClockFaceStyle::centred(width, height, radius);
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
    } else {
        // The time reference is the host system clock (free-running) — the value
        // only feeds the a11y badge, which a standalone clock source does not draw.
        let time_ref = TimeRef::new(RefSource::System, RefStatus::Freerun);
        let face = if twelve_hour {
            ClockFace::digital_12h()
        } else {
            ClockFace::digital_24h()
        };
        let model = ClockModel::new(face, zone, time_ref);
        let text = model.render_digital(now).ok_or(SynthError::ClockTime)?;
        let mut engine = TextEngine::new().map_err(cc)?;
        let size_px = f32_of(height) / 5.0;
        let color = OverlayColor::opaque(0.95, 0.95, 0.95);
        let run = engine
            .rasterize_run(&text, FontFamily::Mono, size_px, [0.95, 0.95, 0.95, 1.0])
            .map_err(cc)?;
        // Centre the run: measure its glyph extent, then offset every glyph.
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
        let (off_x, off_y) = if max_x >= min_x && max_y >= min_y {
            let run_w = max_x.saturating_sub(min_x);
            let run_h = max_y.saturating_sub(min_y);
            (
                (i32_of(width).saturating_sub(run_w)) / 2 - min_x,
                (i32_of(height).saturating_sub(run_h)) / 2 - min_y,
            )
        } else {
            (0, 0)
        };
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

    apply_overlays_to_nv12(&bg, &list, canvas).map_err(cc)
}

#[cfg(not(feature = "overlay"))]
fn render_clock(
    _width: u32,
    _height: u32,
    _canvas: CanvasColor,
    _analog: bool,
    _twelve_hour: bool,
    _tz_offset_minutes: i32,
    _now: WallTime,
) -> Result<Nv12Image, SynthError> {
    Err(SynthError::OverlayRequired)
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
        let key = render_key(kind, now);
        let frame = match &cached {
            Some((k, f)) if *k == key => Arc::clone(f),
            _ => match render(kind, width, height, canvas, now) {
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
            SyntheticKind::Clock { analog: true, .. }
        ));
        // A decoded kind is not synthetic.
        let doc = "schema_version=1\n[canvas]\nwidth=64\nheight=64\nfps=\"25/1\"\npixel_format=\"nv12\"\nbackground=\"#101014\"\n[canvas.color]\nprofile=\"sdr-bt709-limited\"\n[layout]\nkind=\"grid\"\ncolumns=[\"1fr\"]\nrows=[\"1fr\"]\nareas=[\"a\"]\n[[sources]]\nid=\"s\"\nkind=\"rtsp\"\nurl=\"rtsp://x/y\"\n[[cells]]\nid=\"c\"\narea=\"a\"\n[cells.source]\ninput_id=\"s\"\n[[outputs]]\nkind=\"hls\"\npath=\"/tmp/x.m3u8\"\ncodec=\"mpeg2video\"\nsegment_ms=1000\n";
        let cfg = MultiviewConfig::load_from_toml(doc).expect("parse");
        assert!(SyntheticKind::from_source_kind(&cfg.sources[0].kind).is_none());
    }

    #[test]
    fn bars_render_has_the_descending_luma_staircase() {
        let img = render(
            SyntheticKind::Bars,
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
            SyntheticKind::Solid {
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

    #[cfg(feature = "overlay")]
    #[test]
    fn analog_clock_renders_and_animates() {
        let c = canvas();
        // 03:00:00 UTC and 09:00:00 UTC must differ (the hands moved).
        let three = render(
            SyntheticKind::Clock {
                analog: true,
                twelve_hour: false,
                tz_offset_minutes: 0,
            },
            240,
            240,
            c,
            WallTime::from_unix_seconds(3 * 3600),
        )
        .expect("clock 3:00");
        let nine = render(
            SyntheticKind::Clock {
                analog: true,
                twelve_hour: false,
                tz_offset_minutes: 0,
            },
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
                SyntheticKind::Clock {
                    analog: true,
                    twelve_hour,
                    tz_offset_minutes: 0,
                },
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
            SyntheticKind::Clock {
                analog: false,
                twelve_hour: false,
                tz_offset_minutes: 0,
            },
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
}
