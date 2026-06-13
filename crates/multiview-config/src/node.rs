//! The `multiview node` minimal configuration (DEV-B5 / [ADR-0045]).
//!
//! A display node is **a normal run with one input and display outputs**: the
//! node document here carries exactly what a dedicated decode-and-present box
//! needs — one supervised ingest (RTSP/SRT/HLS/MPEG-TS), one or more display
//! heads (the [`crate::Output::Display`] connector/mode surface of ADR-0044),
//! a per-head audio flag, the slate shown on signal loss, the DEV-C2 timing
//! knob, and the hotplug polling fallback cadence — and **lowers** into a full
//! [`MultiviewConfig`] via [`NodeConfig::to_multiview_config`]: one managed
//! source → one absolutely-placed full-canvas cell → one `Output::Display`
//! per head, no control plane. The node thereby reuses the *unchanged*
//! `multiview-input` pacer/jitter/normalize/supervised-reconnect stack and the
//! framestore Live→Stale→Reconnecting→NoSignal ladder (last-good, then the
//! configured local slate) — node resilience equals product resilience by
//! construction, never by re-implementation.
//!
//! The `timing.link_offset_ms` knob is **recorded and validated here but not
//! yet consumed**: the epoch + link-offset frame chooser is DEV-C2 (consuming
//! the DEV-C1 outbound presentation epoch). Until that lands, the node
//! presents through the display sink's existing repeat/drop reconciliation
//! (ADR-0044 §1), which this knob does not alter.
//!
//! [ADR-0045]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0045.md

use serde::{Deserialize, Serialize};

use crate::audio::{OutputAudio, OutputAudioMode};
use crate::error::ConfigError;
use crate::failover::{default_failover_slate, FailoverSlate};
use crate::schema::{
    Canvas, CanvasColor, Cell, CellSource, DisplayModeSpec, Fps, Layout, Output, Rect, Source,
    SourceKind,
};
use crate::MultiviewConfig;

/// The largest accepted `timing.link_offset_ms`: a receiver-side delay beyond
/// ten seconds is a configuration mistake, not a deployment (the documented
/// envelope is 100–300 ms — display-out §8).
const MAX_LINK_OFFSET_MS: u32 = 10_000;

/// The accepted `hotplug.poll_secs` range (the brief's 2–5 s recommendation
/// with headroom either side; the kernel itself polls non-HPD connectors at
/// 10 s).
const POLL_SECS_RANGE: std::ops::RangeInclusive<u64> = 1..=60;

/// The canvas the node composites at when neither an explicit `[canvas]` nor
/// any head mode pins one: 1920×1080 @ 60/1.
const DEFAULT_CANVAS: (u32, u32, i64, i64) = (1_920, 1_080, 60, 1);

/// One supervised ingest, tagged by `kind` (adjacent tagging is the project
/// convention — never `untagged`). The kinds mirror the product's
/// [`SourceKind`] URL variants the full pipeline already ingests with
/// pacing/jitter/normalization/supervised reconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum NodeIngest {
    /// RTSP/RTP pull (`rtsp://…`) — the preferred low-latency node feed.
    Rtsp {
        /// The stream URL (IPv6 literals bracketed, e.g. `rtsp://[2001:db8::10]:8554/program`).
        url: String,
    },
    /// SRT caller/listener (`srt://…`).
    Srt {
        /// The SRT URL.
        url: String,
    },
    /// HLS pull (`http://…` / `https://…`) — accepted, with HLS's
    /// seconds-class latency.
    Hls {
        /// The playlist URL.
        url: String,
    },
    /// Raw MPEG-TS (`udp://…`, `http://…`, …).
    Ts {
        /// The transport-stream URL.
        url: String,
    },
}

impl NodeIngest {
    /// The ingest URL as written.
    #[must_use]
    pub fn url(&self) -> &str {
        match self {
            Self::Rtsp { url } | Self::Srt { url } | Self::Hls { url } | Self::Ts { url } => url,
        }
    }

    /// The kind token, for diagnostics.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Rtsp { .. } => "rtsp",
            Self::Srt { .. } => "srt",
            Self::Hls { .. } => "hls",
            Self::Ts { .. } => "ts",
        }
    }

    /// Validate the URL against the declared kind: non-empty and carrying the
    /// scheme the kind implies (a `srt` ingest with an `rtsp://` URL is a
    /// config mistake the run would only discover at open time).
    fn validate(&self) -> Result<(), ConfigError> {
        let url = self.url().trim();
        if url.is_empty() {
            return Err(ConfigError::Validation(format!(
                "ingest ({}): url must not be empty",
                self.kind_name()
            )));
        }
        let lower = url.to_ascii_lowercase();
        let ok = match self {
            Self::Rtsp { .. } => lower.starts_with("rtsp://") || lower.starts_with("rtsps://"),
            Self::Srt { .. } => lower.starts_with("srt://"),
            Self::Hls { .. } => lower.starts_with("http://") || lower.starts_with("https://"),
            Self::Ts { .. } => lower.contains("://"),
        };
        if !ok {
            return Err(ConfigError::Validation(format!(
                "ingest ({kind}): url {url:?} does not carry a scheme usable for kind \
                 `{kind}` (rtsp ⇒ rtsp://; srt ⇒ srt://; hls ⇒ http(s)://; ts ⇒ any \
                 `scheme://`)",
                kind = self.kind_name()
            )));
        }
        Ok(())
    }

    /// Lower to the matching product [`SourceKind`].
    fn to_source_kind(&self) -> SourceKind {
        match self {
            Self::Rtsp { url } => SourceKind::Rtsp {
                url: url.clone(),
                rtsp: None,
            },
            Self::Srt { url } => SourceKind::Srt { url: url.clone() },
            Self::Hls { url } => SourceKind::Hls { url: url.clone() },
            Self::Ts { url } => SourceKind::Ts { url: url.clone() },
        }
    }
}

/// One display head: the connector to drive plus the ADR-0044 mode surface
/// ([`DisplayModeSpec`] override XOR CVT-RB `forced_mode` for EDID-less
/// chains) and the audio flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeDisplay {
    /// KMS connector name (`HDMI-A-1`, `DP-1`, …); `"auto"` selects the first
    /// connected connector and is only accepted on a single-head node.
    #[serde(default = "default_connector")]
    pub connector: String,
    /// Optional explicit EDID-mode override (mutually exclusive with
    /// `forced_mode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<DisplayModeSpec>,
    /// CVT-RB forced mode for an **EDID-less** connector (mutually exclusive
    /// with `mode`). A forced-mode head has no ELD and therefore no audio
    /// path (display-out §5/§6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_mode: Option<DisplayModeSpec>,
    /// Enable HDMI/DP audio on this head (ELD-gated at runtime; the program
    /// bus, never selectable discrete tracks).
    #[serde(default)]
    pub audio: bool,
}

/// Serde default for [`NodeDisplay::connector`].
fn default_connector() -> String {
    "auto".to_owned()
}

/// Optional explicit composite canvas. Absent ⇒ derived from the first head's
/// `mode`/`forced_mode`, else 1920×1080 @ 60/1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeCanvas {
    /// Width in pixels (`> 0`).
    pub width: u32,
    /// Height in pixels (`> 0`).
    pub height: u32,
    /// Cadence as an exact rational string (never a float — invariant #3).
    pub fps: Fps,
}

/// Presentation-timing knobs (DEV-C2 surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeTiming {
    /// The fixed per-deployment receiver-side delay, in milliseconds, added to
    /// the program epoch when choosing the frame for a vblank (AES67's link
    /// offset applied to video — uniformity across nodes matters, not
    /// smallness). **Recorded and validated today; consumed by the DEV-C2
    /// epoch frame chooser.** Until DEV-C2 lands the node presents through
    /// the display sink's existing repeat/drop reconciliation, which this
    /// value does not alter.
    #[serde(default)]
    pub link_offset_ms: u32,
}

/// Connector-hotplug detection knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeHotplug {
    /// The `force_probe` polling cadence, in seconds, used **only** when the
    /// kernel netlink uevent group is unavailable (rootless containers —
    /// ADR-0045). With kernel uevents available, hotplug is event-driven and
    /// this knob is unused.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
}

impl Default for NodeHotplug {
    fn default() -> Self {
        Self {
            poll_secs: default_poll_secs(),
        }
    }
}

/// Serde default for [`NodeHotplug::poll_secs`].
const fn default_poll_secs() -> u64 {
    5
}

/// Optional controller-enrollment knobs (DEV-B6, ADR-0045 §9).
///
/// **Additive and entirely optional**: a node with no `[controller]` block runs
/// exactly as DEV-B5 (decode-and-present, no management plane). When present, a
/// background task (off the engine/output path — invariant #10) generates and
/// persists an Ed25519 keypair, enrolls against the controller (presenting a
/// one-time `token` for zero-touch, else showing a pairing card), and
/// heartbeats — so the node becomes a managed `displaynode` device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeController {
    /// The controller's API base URL, IPv6-first (bracketed literals, e.g.
    /// `https://[fd00:db8::1]:8080`). The node POSTs `/api/v1/devices/enroll`
    /// and `/api/v1/devices/{id}/heartbeat` against this base.
    pub url: String,
    /// A one-time enrollment token (`<id>.<secret>`) for **zero-touch**
    /// enrollment (the operator minted it in Settings → Display Nodes). Absent
    /// ⇒ the node falls to **screen pairing** (shows a code/QR and waits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enrollment_token: Option<String>,
    /// Where the node persists its Ed25519 keypair (the enrolled identity).
    /// Generated on first start if absent; reused across reboots so a re-enroll
    /// maps to the SAME device. Defaults to `node-identity.key` beside the
    /// config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_path: Option<std::path::PathBuf>,
    /// The human-friendly node name reported at enrollment (the device's
    /// display name). Defaults to the host name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
}

impl NodeController {
    /// Validate the controller block: a non-empty, scheme-bearing URL.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] when the URL is empty or carries no
    /// `scheme://` (so an enrollment attempt would only fail at connect time).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err(ConfigError::Validation(
                "controller: url must not be empty (e.g. \"https://[fd00:db8::1]:8080\")"
                    .to_owned(),
            ));
        }
        if !url.contains("://") {
            return Err(ConfigError::Validation(format!(
                "controller: url {url:?} must carry a scheme (http:// or https://)"
            )));
        }
        if let Some(token) = &self.enrollment_token {
            if token.trim().is_empty() {
                return Err(ConfigError::Validation(
                    "controller: enrollment_token must not be empty (omit it to use screen \
                     pairing instead)"
                        .to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// The `multiview node` configuration document (TOML).
///
/// `deny_unknown_fields` on the document **root** (matching every sub-table):
/// a typo'd top-level section (`[cnavas]`) is a loud parse error naming the
/// offender, never a silent fall-back to the defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct NodeConfig {
    /// Document schema version (defaults to 1).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The one supervised ingest this node decodes and presents.
    pub ingest: NodeIngest,
    /// The display head(s) to light (at least one).
    #[serde(default)]
    pub displays: Vec<NodeDisplay>,
    /// Optional explicit composite canvas (see [`NodeCanvas`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<NodeCanvas>,
    /// Presentation-timing knobs (see [`NodeTiming`]).
    #[serde(default)]
    pub timing: NodeTiming,
    /// Hotplug-detection knobs (see [`NodeHotplug`]).
    #[serde(default)]
    pub hotplug: NodeHotplug,
    /// What the head shows once the tile ladder reaches its down state:
    /// last-good first, then this slate (default: SMPTE/EBU bars).
    #[serde(default = "default_failover_slate")]
    pub on_loss: FailoverSlate,
    /// Optional controller-enrollment knobs (DEV-B6, ADR-0045 §9). Absent ⇒
    /// the node runs unenrolled (exactly the DEV-B5 behaviour).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<NodeController>,
}

/// Serde default for [`NodeConfig::schema_version`].
const fn default_schema_version() -> u32 {
    1
}

/// Whether `text` is shaped like a **node** document rather than an engine
/// document: a parseable TOML body with a top-level `ingest` table (the node
/// schema's one mandatory section; engine documents have none). Malformed
/// TOML returns `false` — the caller then reports it through the engine
/// parse path.
#[must_use]
pub fn is_node_document(text: &str) -> bool {
    toml::from_str::<toml::Value>(text)
        .ok()
        .and_then(|value| value.get("ingest").cloned())
        .is_some()
}

impl NodeConfig {
    /// Parse a node configuration document from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::TomlParse`] if the text is not valid TOML
    /// matching the schema (a float `fps` fails here — invariant #3).
    pub fn load_from_toml(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::TomlParse(e.to_string()))
    }

    /// Validate the document: schema version, ingest URL/scheme, at least one
    /// display head, per-head `mode` XOR `forced_mode` with positive geometry,
    /// unique connectors (`auto` only on a single-head node), and bounded
    /// timing/hotplug knobs.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the offending field.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version == 0 {
            return Err(ConfigError::Validation(
                "schema_version must be >= 1".to_owned(),
            ));
        }
        self.ingest.validate()?;
        if self.displays.is_empty() {
            return Err(ConfigError::Validation(
                "a node needs at least one [[displays]] head to present on".to_owned(),
            ));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(self.displays.len());
        for display in &self.displays {
            let connector = display.connector.trim();
            if connector.is_empty() {
                return Err(ConfigError::Validation(
                    "displays: connector must not be empty (use \"auto\" or a kernel \
                     connector name like \"HDMI-A-1\")"
                        .to_owned(),
                ));
            }
            if connector.eq_ignore_ascii_case("auto") && self.displays.len() > 1 {
                return Err(ConfigError::Validation(
                    "displays: connector \"auto\" (first connected) is only accepted on a \
                     single-head node — name each connector explicitly on a multi-head node"
                        .to_owned(),
                ));
            }
            if seen.iter().any(|prev| prev.eq_ignore_ascii_case(connector)) {
                return Err(ConfigError::Validation(format!(
                    "displays: connector {connector:?} is configured more than once"
                )));
            }
            seen.push(connector);
            if display.mode.is_some() && display.forced_mode.is_some() {
                return Err(ConfigError::Validation(format!(
                    "displays ({connector}): `mode` and `forced_mode` are mutually \
                     exclusive — `mode` selects an EDID-advertised timing, `forced_mode` \
                     computes a CVT-RB timing for an EDID-less chain"
                )));
            }
            for (label, spec) in [
                ("mode", display.mode.as_ref()),
                ("forced_mode", display.forced_mode.as_ref()),
            ] {
                if let Some(spec) = spec {
                    validate_mode_spec(connector, label, spec)?;
                }
            }
        }
        if let Some(canvas) = &self.canvas {
            if canvas.width == 0 || canvas.height == 0 {
                return Err(ConfigError::Validation(format!(
                    "canvas: geometry {}x{} must be positive in both dimensions",
                    canvas.width, canvas.height
                )));
            }
            validate_positive_fps("canvas", canvas.fps)?;
        }
        if self.timing.link_offset_ms > MAX_LINK_OFFSET_MS {
            return Err(ConfigError::Validation(format!(
                "timing: link_offset_ms {} is out of range (0..={MAX_LINK_OFFSET_MS})",
                self.timing.link_offset_ms
            )));
        }
        if !POLL_SECS_RANGE.contains(&self.hotplug.poll_secs) {
            return Err(ConfigError::Validation(format!(
                "hotplug: poll_secs {} is out of range ({}..={})",
                self.hotplug.poll_secs,
                POLL_SECS_RANGE.start(),
                POLL_SECS_RANGE.end()
            )));
        }
        if let Some(controller) = &self.controller {
            controller.validate()?;
        }
        Ok(())
    }

    /// The composite canvas the node runs at: the explicit `[canvas]` when
    /// given, else the first head's `mode`/`forced_mode` geometry + refresh
    /// (the composite is then 1:1 with the scanout raster), else 1920×1080 @
    /// 60/1.
    #[must_use]
    pub fn canvas_geometry(&self) -> (u32, u32, Fps) {
        if let Some(canvas) = &self.canvas {
            return (canvas.width, canvas.height, canvas.fps);
        }
        for display in &self.displays {
            if let Some(spec) = display.mode.as_ref().or(display.forced_mode.as_ref()) {
                return (spec.width, spec.height, spec.refresh);
            }
        }
        let (w, h, num, den) = DEFAULT_CANVAS;
        (
            w,
            h,
            Fps::from(multiview_core::time::Rational::new(num, den)),
        )
    }

    /// Lower this node document into a full [`MultiviewConfig`]: one managed
    /// source (`ingest`) → one absolutely-placed full-canvas cell (`program`,
    /// carrying [`NodeConfig::on_loss`]) → one [`Output::Display`] per head
    /// (`display-0`, `display-1`, …) — no control plane (node
    /// enrollment/management is DEV-B6, and a node must not silently open a
    /// listener nobody configured). The lowered document is validated before
    /// being returned, so the node runs the standard pipeline with a config
    /// that would equally run under `multiview run`.
    ///
    /// The `timing`/`hotplug` knobs do **not** lower: they configure the node
    /// runner itself (the DEV-C2 frame chooser and the hotplug watcher), not
    /// the engine document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] if this document (or, defensively,
    /// the lowered document) fails validation.
    pub fn to_multiview_config(&self) -> Result<MultiviewConfig, ConfigError> {
        self.validate()?;
        let (width, height, fps) = self.canvas_geometry();
        let outputs = self
            .displays
            .iter()
            .enumerate()
            .map(|(i, display)| Output::Display {
                id: Some(format!("display-{i}")),
                connector: display.connector.clone(),
                mode: display.mode.clone(),
                forced_mode: display.forced_mode.clone(),
                gpu_pin: None,
                audio: display.audio.then(|| OutputAudio {
                    mode: OutputAudioMode::Program,
                    tracks: Vec::new(),
                }),
            })
            .collect();
        let lowered = MultiviewConfig {
            schema_version: 1,
            canvas: Canvas {
                width,
                height,
                fps,
                pixel_format: "nv12".to_owned(),
                // Letterbox bars (aspect-mismatched feeds under `contain`)
                // show black, as a display appliance should.
                background: "#000000".to_owned(),
                color: CanvasColor {
                    profile: "sdr-bt709-limited".to_owned(),
                    primaries: None,
                    transfer: None,
                    matrix: None,
                    range: None,
                },
            },
            layout: Layout::Absolute,
            sources: vec![Source {
                id: "ingest".to_owned(),
                display_name: None,
                kind: self.ingest.to_source_kind(),
                auth: None,
                color_override: None,
                captions: None,
                gpu_pin: None,
                wallclock: None,
            }],
            cells: vec![Cell {
                id: "program".to_owned(),
                area: None,
                rect: Some(Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                }),
                z: 0,
                // `contain` letterboxes an aspect-mismatched feed instead of
                // distorting it; a feed matching the canvas aspect fills it
                // exactly.
                fit: Some("contain".to_owned()),
                align: None,
                opacity: None,
                corner_radius: None,
                scaler: None,
                visible: None,
                static_friendly: None,
                border: None,
                qos: None,
                on_loss: self.on_loss,
                source: CellSource {
                    input_id: Some("ingest".to_owned()),
                    kind: None,
                    name: None,
                    url: None,
                    fallback: None,
                },
            }],
            overlays: Vec::new(),
            outputs,
            probes: Vec::new(),
            tally_profiles: Vec::new(),
            salvos: Vec::new(),
            walls: Vec::new(),
            devices: Vec::new(),
            sync_groups: Vec::new(),
            discovery: None,
            control: None,
            placement: None,
            audio: None,
            routing: None,
            // The node's own `timing.link_offset_ms` is the DEV-C2
            // RECEIVER-side frame-chooser knob (recorded above, not yet
            // consumed — see the module doc); the engine document's
            // `[timing]` block is the DEV-C1 OUTBOUND presentation-epoch
            // policy. They are distinct surfaces, so the lowering leaves the
            // outbound block absent (the engine's documented default).
            timing: None,
        };
        lowered.validate()?;
        Ok(lowered)
    }
}

/// Validate one [`DisplayModeSpec`]: positive geometry and a positive exact
/// refresh.
fn validate_mode_spec(
    connector: &str,
    label: &str,
    spec: &DisplayModeSpec,
) -> Result<(), ConfigError> {
    if spec.width == 0 || spec.height == 0 {
        return Err(ConfigError::Validation(format!(
            "displays ({connector}): {label} geometry {}x{} must be positive in both \
             dimensions",
            spec.width, spec.height
        )));
    }
    validate_positive_fps(label, spec.refresh)
}

/// Validate that an exact-rational rate is positive.
fn validate_positive_fps(label: &str, fps: Fps) -> Result<(), ConfigError> {
    let r = fps.rational();
    if r.num <= 0 || r.den <= 0 {
        return Err(ConfigError::Validation(format!(
            "{label}: rate {}/{} must be positive",
            r.num, r.den
        )));
    }
    Ok(())
}

#[cfg(test)]
mod controller_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::NodeConfig;

    /// A minimal valid node document (one ingest, one head) the controller-block
    /// tests append a `[controller]` table to.
    const BASE: &str = r#"
[ingest]
kind = "rtsp"
url = "rtsp://[2001:db8::10]:8554/program"
[[displays]]
connector = "HDMI-A-1"
"#;

    #[test]
    fn a_node_without_a_controller_block_is_unenrolled() {
        // The DEV-B5 baseline: no `[controller]` block, so the node runs
        // decode-and-present with no management plane.
        let cfg = NodeConfig::load_from_toml(BASE).expect("the baseline node parses");
        cfg.validate().expect("the baseline node validates");
        assert!(
            cfg.controller.is_none(),
            "a node without [controller] is unenrolled (additive — DEV-B5 unchanged)"
        );
    }

    #[test]
    fn a_controller_block_parses_and_validates() {
        let doc = format!(
            "{BASE}\n[controller]\nurl = \"https://[fd00:db8::1]:8080\"\n\
             enrollment_token = \"enr-abc.def\"\nnode_name = \"Lobby left\"\n"
        );
        let cfg = NodeConfig::load_from_toml(&doc).expect("the controller node parses");
        cfg.validate().expect("the controller node validates");
        let controller = cfg.controller.expect("the controller block is present");
        assert_eq!(controller.url, "https://[fd00:db8::1]:8080");
        assert_eq!(controller.enrollment_token.as_deref(), Some("enr-abc.def"));
        assert_eq!(controller.node_name.as_deref(), Some("Lobby left"));
    }

    #[test]
    fn a_controller_with_no_token_is_the_pairing_path() {
        // No `enrollment_token` ⇒ screen pairing; still a valid document.
        let doc = format!("{BASE}\n[controller]\nurl = \"https://[fd00:db8::1]:8080\"\n");
        let cfg = NodeConfig::load_from_toml(&doc).expect("parses");
        cfg.validate().expect("validates");
        assert!(cfg.controller.unwrap().enrollment_token.is_none());
    }

    #[test]
    fn an_empty_controller_url_is_rejected() {
        let doc = format!("{BASE}\n[controller]\nurl = \"\"\n");
        let cfg = NodeConfig::load_from_toml(&doc).expect("parses (validation is separate)");
        let err = cfg.validate().expect_err("an empty controller url is rejected");
        assert!(err.to_string().contains("controller"), "{err}");
    }

    #[test]
    fn a_schemeless_controller_url_is_rejected() {
        let doc = format!("{BASE}\n[controller]\nurl = \"fd00:db8::1\"\n");
        let cfg = NodeConfig::load_from_toml(&doc).expect("parses");
        let err = cfg.validate().expect_err("a schemeless controller url is rejected");
        assert!(err.to_string().contains("scheme"), "{err}");
    }

    #[test]
    fn an_unknown_controller_field_is_rejected_naming_it() {
        let doc = format!(
            "{BASE}\n[controller]\nurl = \"https://[fd00:db8::1]:8080\"\nuurl = \"x\"\n"
        );
        let err = NodeConfig::load_from_toml(&doc)
            .expect_err("an unknown controller field is a parse error");
        assert!(err.to_string().contains("uurl"), "{err}");
    }
}
