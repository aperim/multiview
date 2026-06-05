//! The serde schema for a Multiview config document.
//!
//! These types mirror the authored TOML (and the canonical JSON wire form)
//! exactly — see `docs/templates/layout-and-config.md` and the shipped
//! `examples/*.toml`. The shape is: a top-level `schema_version`, a `[canvas]`
//! (with `fps` as an exact rational **string**, never a float — invariant #3),
//! a `[layout]` (CSS-grid / absolute / preset), tagged `[[sources]]`,
//! `[[cells]]`, `[[overlays]]`, and `[[outputs]]`.
//!
//! All unions are **internally tagged** by `kind` (`#[serde(tag = "kind")]`),
//! never `untagged`: that is the only encoding robust across non-self-describing
//! TOML and JSON (ADR-0010).

use std::fmt;
use std::str::FromStr;

use multiview_core::time::Rational;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::ConfigError;
use crate::grid::{GridLayout, Track};

/// An exact frame rate parsed from a `"num/den"` string (e.g. `"30000/1001"`).
///
/// A bare TOML/JSON float (e.g. `29.97`) deliberately fails to deserialize:
/// frame rates are exact rationals, never floats (invariant #3). The value is
/// carried as a [`Rational`] and re-serialized back to its `"num/den"` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fps(Rational);

impl Fps {
    /// The underlying exact [`Rational`] cadence.
    #[must_use]
    pub const fn rational(self) -> Rational {
        self.0
    }
}

impl FromStr for Fps {
    type Err = ConfigError;

    /// Parse `"num/den"` into an exact frame rate.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidFps`] if the string is not exactly two
    /// integers separated by a single `/`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        let mut parts = trimmed.split('/');
        let (Some(num_str), Some(den_str), None) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(ConfigError::InvalidFps {
                value: trimmed.to_owned(),
                reason: "expected exactly one '/' separating numerator and denominator".to_owned(),
            });
        };
        let num: i64 = num_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidFps {
                value: trimmed.to_owned(),
                reason: "numerator is not an integer".to_owned(),
            })?;
        let den: i64 = den_str
            .trim()
            .parse()
            .map_err(|_| ConfigError::InvalidFps {
                value: trimmed.to_owned(),
                reason: "denominator is not an integer".to_owned(),
            })?;
        Ok(Self(Rational::new(num, den)))
    }
}

impl fmt::Display for Fps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.0.num, self.0.den)
    }
}

impl Serialize for Fps {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Fps {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Visitor that accepts only strings (a float would be a wrong type).
        struct FpsVisitor;
        impl Visitor<'_> for FpsVisitor {
            type Value = Fps;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a rational frame-rate string like \"30000/1001\"")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(de::Error::custom)
            }
        }
        deserializer.deserialize_str(FpsVisitor)
    }
}

/// Per-axis color override on a source (each axis `auto` or an explicit token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ColorOverride {
    /// Primaries axis (`auto` or an explicit primaries token).
    #[serde(default = "auto_token")]
    pub primaries: String,
    /// Transfer axis.
    #[serde(default = "auto_token")]
    pub transfer: String,
    /// Matrix axis.
    #[serde(default = "auto_token")]
    pub matrix: String,
    /// Range axis.
    #[serde(default = "auto_token")]
    pub range: String,
}

/// The default `"auto"` token for an unspecified color-override axis.
fn auto_token() -> String {
    "auto".to_owned()
}

/// The canvas working-color-space block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CanvasColor {
    /// Working-color-space profile name (e.g. `sdr-bt709-limited`, `custom`).
    pub profile: String,
    /// Explicit primaries (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primaries: Option<String>,
    /// Explicit transfer (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<String>,
    /// Explicit matrix (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matrix: Option<String>,
    /// Explicit range (only when `profile = "custom"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
}

/// The output canvas: geometry, cadence, pixel format, background, color space.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Canvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Output cadence as an exact rational (parsed from a `"num/den"` string).
    pub fps: Fps,
    /// Working pixel format (`nv12` 8-bit, `p010` 10-bit).
    pub pixel_format: String,
    /// Background fill (hex color, e.g. `#101014`).
    pub background: String,
    /// Working color space.
    pub color: CanvasColor,
}

/// RTSP-specific ingest options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RtspOptions {
    /// Lower-transport selection (`tcp` / `udp`).
    pub transport: String,
}

/// Reference-only credential pointer for a source (never plaintext).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SourceAuth {
    /// A secret reference (e.g. `op://Servers/cam/credentials`).
    pub secret_ref: String,
}

/// The face a [`SourceKind::Clock`] source renders (config-level mirror of the
/// overlay clock face; mapped onto `multiview_overlay`'s model at render time).
///
/// A plain string enum (`analog` / `digital`); the digital-only `twelve_hour`
/// flag rides alongside it on the [`SourceKind::Clock`] payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockFaceConfig {
    /// Analog face: hour / minute / second hands on a ticked bezel.
    #[default]
    Analog,
    /// Digital `HH:MM:SS` readout (see `twelve_hour`).
    Digital,
}

/// The kind-specific payload of a managed input, internally tagged by `kind`.
///
/// Synthetic sources (`bars`, `solid`, `clock`) are produced in-process and are
/// first-class peers of the decoded kinds (ADR-0027): nothing downstream of
/// ingest treats them differently. The network kinds carry a `url`; NDI binds by
/// source `name`; `file` a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceKind {
    /// Built-in SMPTE/EBU colour bars (the line-up signal). `test` is accepted as
    /// a back-compat alias and canonicalizes to `bars`.
    #[serde(alias = "test")]
    Bars,
    /// A solid-colour slate (hex, e.g. `#101014`).
    Solid {
        /// Fill colour as a `#RRGGBB` (or `#RGB`) hex string.
        color: String,
    },
    /// A full-frame clock disciplined by the system wall clock.
    Clock {
        /// Analog (default) or digital face.
        #[serde(default)]
        face: ClockFaceConfig,
        /// `true` for a 12-hour digital readout with AM/PM; ignored for analog.
        #[serde(default)]
        twelve_hour: bool,
        /// Timezone offset from UTC in minutes (e.g. `600` = UTC+10). Real
        /// offsets span `-720..=840`.
        #[serde(default)]
        tz_offset_minutes: i32,
    },
    /// RTSP pull.
    Rtsp {
        /// Source URL.
        url: String,
        /// RTSP transport options.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rtsp: Option<RtspOptions>,
    },
    /// HLS / M3U pull.
    Hls {
        /// Playlist URL.
        url: String,
    },
    /// MPEG-TS input.
    Ts {
        /// Source URL.
        url: String,
    },
    /// SRT input.
    Srt {
        /// Source URL.
        url: String,
    },
    /// RTMP input.
    Rtmp {
        /// Source URL.
        url: String,
    },
    /// NDI input, bound by source name.
    Ndi {
        /// NDI source name (e.g. `STUDIO (CAM 1)`).
        name: String,
    },
    /// File input.
    File {
        /// Filesystem path.
        path: String,
    },
}

/// A managed input: a stable `id`, a display name, the kind-specific payload,
/// and optional auth/color overrides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Source {
    /// Stable input id (referenced by `cells.source.input_id`).
    pub id: String,
    /// Human-friendly display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// The kind-specific payload (flattened so `kind`/`url` sit at top level).
    #[serde(flatten)]
    pub kind: SourceKind,
    /// Reference-only credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<SourceAuth>,
    /// Per-source color override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_override: Option<ColorOverride>,
    /// Per-source caption/subtitle selector for native in-pipeline decode.
    ///
    /// Absent means no captions are decoded for this source — the engine never
    /// decodes a track it will not display (an efficiency lever, not a default
    /// cost). See [`CaptionSelector`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captions: Option<CaptionSelector>,
}

/// How captions/subtitles are sourced for one input, decoded **natively from the
/// source stream** (the primary path, superseding external sidecar files).
///
/// Internally tagged by `mode` (robust across TOML and JSON; never `untagged`).
/// Each family maps onto `multiview_ffmpeg`'s `CaptionDecoder` at ingest time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptionSelector {
    /// Auto-select the first usable caption track on the source (surface
    /// whatever the stream carries).
    Auto,
    /// Captions explicitly disabled — equivalent to omitting the field, but
    /// expressible so a template can pin "no captions".
    Off,
    /// DVB teletext, addressed by page (e.g. `801` for English subtitles).
    TeletextPage {
        /// Teletext page number (magazine-addressed, typically `100`–`899`).
        page: u16,
    },
    /// A subtitle track identified by stream id or language tag (e.g. `"eng"`).
    Track {
        /// The track identifier (a language tag or a stream-relative id).
        id: String,
    },
    /// Embedded CEA-608/708 captions carried in the video stream, addressed by
    /// field/service (e.g. `"cc1"`).
    EmbeddedCc {
        /// The caption field/service selector (e.g. `cc1`..`cc4` or a service).
        field: String,
    },
    /// An external sidecar subtitle file (SRT/WebVTT) — the legacy path, kept so
    /// it routes through the same per-tile burn-in as native decode.
    Sidecar {
        /// Filesystem path to the `.srt`/`.vtt` sidecar.
        path: String,
    },
}

/// The layout placement strategy, internally tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Layout {
    /// A named factory preset.
    Preset {
        /// Preset name (`2x2`, `3x3`, `1+5`, `pip`).
        preset: String,
    },
    /// A CSS-grid layout (fr/px/% tracks + areas).
    Grid {
        /// Column tracks.
        columns: Vec<String>,
        /// Row tracks.
        rows: Vec<String>,
        /// Uniform gap in pixels.
        #[serde(default)]
        gap: u32,
        /// Row-gap override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        row_gap: Option<u32>,
        /// Column-gap override.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column_gap: Option<u32>,
        /// `grid-template-areas` map.
        areas: Vec<String>,
    },
    /// Absolute normalized rects (placement carried per-cell).
    Absolute,
}

/// A normalized rectangle (`0.0..=1.0`) for an absolutely-placed cell.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

/// A cell border specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Border {
    /// Border width in pixels.
    #[serde(default)]
    pub width_px: u32,
    /// Border color (hex).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Border style (`solid`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
}

/// The per-cell `QoS` / degradation policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CellQos {
    /// Relative priority (higher is shed last).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// Degradation strategy (`maintain-fps`, `maintain-resolution`, `balanced`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degradation: Option<String>,
}

/// A cell's source binding: a managed `input_id` (preferred) or an inline spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CellSource {
    /// Reference to a managed input by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    /// Inline source kind (`ndi`, `rtmp`, …) when not referencing a managed id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Inline NDI source name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Inline URL (rtmp/rtsp/hls/…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Inline offline fallback behaviour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

/// One cell: placement (by grid `area` or absolute `rect`), fit/z/styling, and
/// a source binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Cell {
    /// Stable cell id.
    pub id: String,
    /// Grid area name (mutually exclusive with `rect`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// Absolute normalized rect (mutually exclusive with `area`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
    /// Stacking order (higher draws on top).
    #[serde(default)]
    pub z: i32,
    /// Fit mode (`fill`/`contain`/`cover`/`none`/`scale_down`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<String>,
    /// Anchor for crop/letterbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<String>,
    /// Opacity (premultiplied, linear).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Corner-radius clip in pixels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corner_radius: Option<u32>,
    /// Scaler selection (`auto`/`bilinear`/`lanczos`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scaler: Option<String>,
    /// Whether the cell is visible (`false` => decode-skip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Hint that the source is largely static.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_friendly: Option<bool>,
    /// Border specification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<Border>,
    /// `QoS` / degradation policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qos: Option<CellQos>,
    /// Source binding.
    pub source: CellSource,
}

/// An overlay layer, internally tagged by `kind`.
///
/// Overlays carry a large, kind-dependent parameter set; the rarely-uniform
/// extras are captured verbatim so the document round-trips losslessly without
/// this crate having to model every overlay kind's fields up front.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Overlay {
    /// Stable overlay id.
    pub id: String,
    /// Overlay kind (`clock`, `tally_border`, `label`, …).
    pub kind: String,
    /// Attachment target (`canvas` or a cell id).
    pub target: String,
    /// Stacking order.
    #[serde(default)]
    pub z: i32,
    /// Kind-specific parameters captured verbatim (lossless round-trip).
    #[serde(flatten)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// An output sink/server, internally tagged by `kind`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Output {
    /// RTSP server.
    RtspServer {
        /// Mount point (e.g. `/multiview`).
        mount: String,
        /// Video codec (`h264`, `hevc`, …).
        codec: String,
        /// Latency profile hint.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latency_profile: Option<String>,
    },
    /// Low-latency HLS packager.
    LlHls {
        /// Output path.
        path: String,
        /// Video codec.
        codec: String,
        /// Target part duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        part_target_ms: Option<u32>,
        /// Segment duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_ms: Option<u32>,
        /// GOP duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gop_ms: Option<u32>,
    },
    /// HLS packager.
    Hls {
        /// Output path.
        path: String,
        /// Video codec.
        codec: String,
        /// Segment duration (ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_ms: Option<u32>,
    },
    /// NDI output.
    Ndi {
        /// NDI source name to advertise.
        name: String,
    },
    /// RTMP push.
    Rtmp {
        /// Destination URL.
        url: String,
        /// Video codec.
        codec: String,
    },
    /// SRT push.
    Srt {
        /// Destination URL.
        url: String,
        /// Video codec.
        codec: String,
    },
}

impl Layout {
    /// Build a [`GridLayout`] (parsed tracks) when this is a grid layout.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidTrack`] if any track string is malformed.
    pub fn as_grid_layout(&self) -> Result<Option<GridLayout>, ConfigError> {
        let Self::Grid {
            columns,
            rows,
            gap,
            row_gap,
            column_gap,
            areas,
        } = self
        else {
            return Ok(None);
        };
        let columns = parse_tracks(columns)?;
        let rows = parse_tracks(rows)?;
        Ok(Some(GridLayout {
            columns,
            rows,
            gap: *gap,
            row_gap: *row_gap,
            column_gap: *column_gap,
            areas: areas.clone(),
        }))
    }
}

/// Parse a list of track strings into [`Track`] values.
fn parse_tracks(tracks: &[String]) -> Result<Vec<Track>, ConfigError> {
    tracks.iter().map(|t| t.parse::<Track>()).collect()
}
