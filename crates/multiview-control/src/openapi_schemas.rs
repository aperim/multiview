//! `OpenAPI` schema mirrors for the `multiview-core` alarm value types
//! (feature `openapi`).
//!
//! The alarm REST surface returns the `serde`-serialised
//! [`multiview_core::alarm::AlarmRecord`] verbatim. That type lives in the frozen
//! `multiview-core` crate and deliberately carries no `utoipa::ToSchema` derive (it
//! has no web dependency), so this module owns the **`OpenAPI` contract** for the
//! alarm body: a set of `ToSchema` structs/enums whose serde shapes match the
//! core types field-for-field and tag-for-tag.
//!
//! These types are used **only** to describe the schema in the generated
//! `OpenAPI` document — the handlers serialise the real core types. A round-trip
//! test (`tests/alarms.rs`) pins the two shapes together so they cannot drift.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `OpenAPI` mirror of [`multiview_core::alarm::PerceivedSeverity`] (X.733).
///
/// Serde-equivalent: a unit enum rendered as its `PascalCase` variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub enum PerceivedSeverityDoc {
    /// The condition has cleared (or never fired): no active alarm.
    Cleared,
    /// Severity could not be determined.
    Indeterminate,
    /// A warning: a potential or impending problem.
    Warning,
    /// Minor: a non-service-affecting fault.
    Minor,
    /// Major: a service-affecting fault requiring urgent attention.
    Major,
    /// Critical: a service-affecting fault requiring immediate attention.
    Critical,
}

/// `OpenAPI` mirror of [`multiview_core::alarm::AlarmKind`] (probe/fault taxonomy).
///
/// Serde-equivalent: a unit enum rendered as its `PascalCase` variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub enum AlarmKindDoc {
    /// Picture black below a luma threshold for the dwell window.
    Black,
    /// Picture frozen for the dwell window.
    Freeze,
    /// Audio silence below a level threshold for the dwell window.
    Silence,
    /// Audio over the permitted ceiling.
    OverLevel,
    /// Audio sample/true-peak clipping.
    Clip,
    /// A channel pair is phase-inverted.
    PhaseInvert,
    /// Integrated/short-term loudness violates the target profile.
    LoudnessViolation,
    /// Captions/subtitles expected but absent.
    CaptionLoss,
    /// Signalled format/standard no longer matches expectation.
    FormatMismatch,
    /// No usable signal on the input.
    SignalLoss,
}

/// `OpenAPI` mirror of [`multiview_core::alarm::AlarmScope`].
///
/// Serde-equivalent: internally tagged on `kind`, `snake_case` variant tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlarmScopeDoc {
    /// A single named probe.
    Probe {
        /// Probe identifier.
        id: String,
    },
    /// A multiview tile, by zero-based index.
    Tile {
        /// Zero-based tile index.
        index: u32,
    },
    /// A named virtual/Boolean alarm group.
    Group {
        /// Group name.
        name: String,
    },
    /// The whole system.
    System,
}

/// `OpenAPI` mirror of [`multiview_core::alarm::AckState`].
///
/// Serde-equivalent: internally tagged on `state`, `PascalCase` variant tags; the
/// acknowledged variant carries `who` and `when` (nanoseconds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "state")]
#[non_exhaustive]
pub enum AckStateDoc {
    /// Not yet acknowledged by an operator.
    Unacked,
    /// Acknowledged: who acknowledged it and when (media-time nanoseconds).
    Acked {
        /// Operator/user identifier that acknowledged the alarm.
        who: String,
        /// When the acknowledgement happened, media-time nanoseconds.
        when: i64,
    },
}

/// `OpenAPI` mirror of [`multiview_core::alarm::AlarmRecord`].
///
/// Serde-equivalent field-for-field. `id` is the alarm id string;
/// `raised_at`/`dwell` are media-time nanoseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AlarmRecordDoc {
    /// Stable identity of this alarm instance.
    pub id: String,
    /// The probe/fault class that raised it.
    pub kind: AlarmKindDoc,
    /// Current perceived severity (X.733).
    pub severity: PerceivedSeverityDoc,
    /// What the alarm applies to.
    pub scope: AlarmScopeDoc,
    /// When the alarm was first raised, media-time nanoseconds.
    pub raised_at: i64,
    /// How long the condition has persisted, media-time nanoseconds.
    pub dwell: i64,
    /// Whether the alarm is latched.
    pub latched: bool,
    /// Operator acknowledgement state.
    pub ack: AckStateDoc,
}

// ---- Tally / salvo / IS-07 mirrors (Wave C operator surface) ----

/// `OpenAPI` mirror of [`multiview_core::tally::TallyColor`] (the TSL UMD palette).
///
/// Serde-equivalent: a unit enum rendered as its `PascalCase` variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub enum TallyColorDoc {
    /// Lamp off (TSL code 0).
    Off,
    /// Red — program / on-air (TSL code 1).
    Red,
    /// Green — preview (TSL code 2).
    Green,
    /// Amber — a third/ISO state (TSL code 3).
    Amber,
}

/// `OpenAPI` mirror of [`multiview_core::tally::BusSource`].
///
/// Serde-equivalent: internally tagged on `kind`, `snake_case` variant tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum BusSourceDoc {
    /// The program / on-air bus.
    Program,
    /// The preview / next bus.
    Preview,
    /// An auxiliary bus, by zero-based index.
    Aux {
        /// Zero-based aux bus index.
        index: u32,
    },
    /// An isolated (ISO) record bus, by zero-based index.
    Iso {
        /// Zero-based ISO bus index.
        index: u32,
    },
}

/// `OpenAPI` mirror of [`multiview_core::tally::TallyState`].
///
/// Serde-equivalent: `brightness` is the bare 2-bit level (`0..=3`) because the
/// core `Brightness` is a newtype that serialises as its inner number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TallyStateDoc {
    /// The lamp colour.
    pub color: TallyColorDoc,
    /// The lamp brightness level (`0..=3`).
    pub brightness: u8,
    /// Which bus this state came from.
    pub source: BusSourceDoc,
}

/// `OpenAPI` mirror of [`multiview_events::TallyTarget`].
///
/// Serde-equivalent: internally tagged on `kind`, `snake_case` variant tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TallyTargetDoc {
    /// A multiview tile, by zero-based index.
    Tile {
        /// Zero-based tile index.
        index: u32,
    },
    /// A named UMD/tally element.
    Element {
        /// Element name.
        name: String,
    },
}

/// `OpenAPI` mirror of [`crate::tally_state::TallyEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TallyEntryDoc {
    /// What the tally state applies to.
    pub target: TallyTargetDoc,
    /// The resolved tally lamp state.
    pub state: TallyStateDoc,
}

/// `OpenAPI` mirror of [`crate::routes::tally::OverrideRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OverrideRequestDoc {
    /// The tally target the override applies to.
    pub target: TallyTargetDoc,
    /// The lamp colour to force.
    pub color: TallyColorDoc,
}

/// `OpenAPI` mirror of [`crate::routes::tally::ClearOverrideRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ClearOverrideRequestDoc {
    /// The tally target whose override is cleared.
    pub target: TallyTargetDoc,
}

/// `OpenAPI` mirror of [`multiview_config::SourceRecall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SourceRecallDoc {
    /// The cell id whose source binding changes.
    pub cell: String,
    /// The managed source id to bind into the cell.
    pub input_id: String,
}

/// `OpenAPI` mirror of [`multiview_config::TallyRecall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TallyRecallDoc {
    /// The cell id whose tally is forced.
    pub cell: String,
    /// The lamp colour to assert.
    pub color: TallyColorDoc,
}

/// `OpenAPI` mirror of [`multiview_config::UmdRecall`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UmdRecallDoc {
    /// The cell id whose UMD label changes.
    pub cell: String,
    /// The label text to display.
    pub text: String,
}

/// `OpenAPI` mirror of [`multiview_config::Salvo`].
///
/// Serde-equivalent field-for-field; the optional fields skip when empty/absent
/// exactly as the config type does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SalvoDoc {
    /// Stable salvo id (unique within the document).
    pub id: String,
    /// Human-friendly display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Recall a named layout (preset or head layout name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// Source rebindings.
    #[serde(default)]
    pub sources: Vec<SourceRecallDoc>,
    /// Forced tally states.
    #[serde(default)]
    pub tally: Vec<TallyRecallDoc>,
    /// UMD label changes.
    #[serde(default)]
    pub umd: Vec<UmdRecallDoc>,
}

/// `OpenAPI` mirror of [`multiview_config::BitColor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BitColorDoc {
    /// The zero-based bit position in the incoming tally word.
    pub bit: u8,
    /// The lamp colour asserted when the bit is set.
    pub color: TallyColorDoc,
}

/// `OpenAPI` mirror of [`multiview_config::IndexCell`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct IndexCellDoc {
    /// The zero-based source/display index in the tally protocol.
    pub index: u32,
    /// The cell id the index resolves to.
    pub cell: String,
}

/// `OpenAPI` mirror of [`multiview_config::TallyProfile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TallyProfileDoc {
    /// Stable profile id (unique within the document).
    pub id: String,
    /// Bit→colour rules.
    #[serde(default)]
    pub bit_colors: Vec<BitColorDoc>,
    /// Protocol-index→cell address map.
    #[serde(default)]
    pub index_cells: Vec<IndexCellDoc>,
}

// ---- Stream-inventory mirrors (RT-3 read-only discovery; ADR-0034 §3/§9) ----
//
// `GET /api/v1/inputs/{id}/streams` returns the serde-serialised
// `multiview_core::stream::StreamInventory` verbatim. Like the alarm/tally
// surfaces above, that type lives in the frozen, web-free `multiview-core`
// crate (no `utoipa::ToSchema`), so these `*Doc` mirrors own the OpenAPI
// contract: their serde shapes match the core types field-for-field and
// tag-for-tag. A round-trip pin test (`tests/input_streams.rs`) keeps them from
// drifting. The handlers serialise the real core type; these are documentation
// only.

/// `OpenAPI` mirror of [`multiview_core::stream::StabilityTier`].
///
/// Serde-equivalent: a unit enum, `snake_case` (`hard` / `soft`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StabilityTierDoc {
    /// A genuinely-stable key (TS PID, HLS group+name): survives a re-probe.
    Hard,
    /// A heuristic key (ordinal-bearing hash): an operator-visible reorder risk.
    Soft,
}

/// `OpenAPI` mirror of [`multiview_core::stream::DataKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataKindDoc {
    /// SCTE-35 ad/splice signalling.
    Scte35,
    /// SMPTE ST 0601 KLV metadata.
    Klv,
}

/// `OpenAPI` mirror of [`multiview_core::stream::TcSourceKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TcSourceKindDoc {
    /// Linear timecode.
    Ltc,
    /// Vertical-interval timecode.
    Vitc,
    /// Ancillary timecode (SMPTE RP 188).
    AtcRp188,
    /// Generated from the output clock (no embedded source timecode).
    Generated,
}

/// `OpenAPI` mirror of [`multiview_core::stream::StreamKind`].
///
/// Serde-equivalent: internally tagged on `kind` with the payload (for the
/// data/timecode variants) under `payload`, `snake_case` tags. This is the same
/// `kind`/`payload` pair the descriptor flattens in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamKindDoc {
    /// A video elementary stream.
    Video,
    /// An audio elementary stream.
    Audio,
    /// A subtitle / caption elementary stream.
    Subtitle,
    /// A data elementary stream (SCTE-35 / KLV) — passthrough, never decoded.
    Data(DataKindDoc),
    /// A timecode elementary stream — carried, not composited.
    Timecode(TcSourceKindDoc),
}

/// `OpenAPI` mirror of [`multiview_core::stream::StreamDetail`].
///
/// Serde-equivalent: adjacently tagged (`detail` / `params`), `snake_case` tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "detail", content = "params", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamDetailDoc {
    /// Video-stream geometry + cadence.
    Video {
        /// Coded width in pixels (`0` if undeclared).
        width: u32,
        /// Coded height in pixels (`0` if undeclared).
        height: u32,
        /// The container's declared average frame rate, if any (exact rational
        /// `[num, den]` — never a float fps, invariant #3).
        frame_rate: Option<[i64; 2]>,
    },
    /// Audio-track layout.
    Audio {
        /// Channel count (`0` if undeclared).
        channels: u16,
        /// Sample rate in Hz (`0` if undeclared).
        sample_rate: u32,
    },
    /// Subtitle / caption track flags.
    Subtitle {
        /// Whether the track is flagged "forced".
        forced: bool,
    },
    /// A passthrough (SCTE-35 / KLV data or timecode) stream — no AV detail.
    Passthrough,
}

/// `OpenAPI` mirror of [`multiview_core::stream::StableStreamId`].
///
/// Serde-equivalent field-for-field: a kind-scope discriminant char, the opaque
/// stable key string, and the stability tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StableStreamIdDoc {
    /// The kind-scope discriminant character (`v`/`a`/`s`/`d`/`t`).
    pub kind_scope: char,
    /// The opaque, stable key string (kind-scope excluded).
    pub key: String,
    /// How stable the key is.
    pub tier: StabilityTierDoc,
}

/// `OpenAPI` mirror of [`multiview_core::stream::StreamDescriptor`].
///
/// Serde-equivalent: the [`StreamKindDoc`] is **flattened** (so a descriptor
/// carries a single `kind` / `payload` pair at its top level) alongside the
/// adjacently-tagged [`StreamDetailDoc`]. `language` is a BCP-47 string or
/// `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StreamDescriptorDoc {
    /// The stable, kind-scoped identity a crosspoint binds to.
    pub id: StableStreamIdDoc,
    /// The canonical media kind (flattened `kind` / `payload`).
    #[serde(flatten)]
    pub kind: StreamKindDoc,
    /// The validated BCP-47 language tag, if the container declared a usable one.
    pub language: Option<String>,
    /// The codec descriptor name (e.g. `h264`, `aac`, `dvbsub`).
    pub codec: String,
    /// The track title / handler name, if declared.
    pub title: Option<String>,
    /// Whether the container flags this stream as default for its kind.
    pub default: bool,
    /// The kind-specific detail.
    pub detail: StreamDetailDoc,
}

/// `OpenAPI` mirror of [`multiview_core::stream::StreamInventory`].
///
/// The full, typed list of every elementary stream an input offers (RT-3). This
/// is the body of `GET /api/v1/inputs/{id}/streams`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StreamInventoryDoc {
    /// The owning input's id, if known.
    pub input_id: Option<String>,
    /// Every elementary stream the input offers, in container order.
    pub streams: Vec<StreamDescriptorDoc>,
}

/// `OpenAPI` mirror of [`multiview_config::routing::StreamSelector`] (RT-4).
///
/// Serde-equivalent: internally tagged on `by`, `snake_case` tags — never
/// `untagged` (ADR-0010). Picks **which** elementary stream of a kind a
/// [`StreamRefDoc`] addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "by", rename_all = "snake_case")]
#[non_exhaustive]
pub enum StreamSelectorDoc {
    /// Select the stream at this 0-based index within its kind.
    Index {
        /// The 0-based index among same-kind streams.
        index: usize,
    },
    /// Select the first stream whose BCP-47 / ISO-639 language tag matches.
    Language {
        /// The requested BCP-47 / ISO-639 language tag.
        language: String,
    },
    /// Let the input pick its best stream of the kind (the default).
    Best,
    /// Select by a stable, kind-scoped id (`StableStreamId`).
    StreamId {
        /// The stable stream id string (e.g. `v/pid:256`).
        id: String,
    },
}

/// `OpenAPI` mirror of [`multiview_config::routing::StreamRef`] (RT-4 / RT-11).
///
/// A reference to **one elementary stream of one input** — the source side of a
/// crosspoint take. Serde-equivalent: `kind` is a **nested** adjacently-tagged
/// [`StreamKindDoc`] object (`kind = { kind = "video" }`), not flattened — exactly
/// as `StreamRef` serialises (a pinned round-trip test guards against drift).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StreamRefDoc {
    /// The managed input id this stream comes from.
    pub input_id: String,
    /// The canonical kind of the elementary stream (nested `kind`/`payload`).
    pub kind: StreamKindDoc,
    /// Which stream of that kind to use (absent ⇒ `best`).
    #[serde(default = "default_best_selector")]
    pub selector: StreamSelectorDoc,
}

/// The default selector for [`StreamRefDoc`] (`best`), mirroring the config
/// default.
fn default_best_selector() -> StreamSelectorDoc {
    StreamSelectorDoc::Best
}

/// `OpenAPI` mirror of [`multiview_control::routing::RouteTarget`] (RT-11).
///
/// The destination a crosspoint take re-points — internally tagged on `kind`,
/// `snake_case`, never `untagged`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RouteTargetDoc {
    /// A layout video cell.
    VideoCell {
        /// The destination cell id.
        cell: String,
    },
    /// The mixed program-bus channel (absorbs the source layout).
    AudioProgramBus {
        /// The program-bus channel name.
        channel: String,
    },
    /// A named discrete output track (its layout is pinned for the session).
    AudioDiscreteTrack {
        /// The discrete-track name.
        track: String,
        /// The pinned channel count, if known (the classifier's compare target).
        #[serde(default)]
        pinned_channels: Option<u16>,
    },
    /// A subtitle / caption layer.
    SubtitleLayer {
        /// The destination layer id.
        layer: String,
    },
}

/// `OpenAPI` mirror of [`multiview_events::WarningSeverity`] (SA-0).
///
/// Serde-equivalent: a unit enum rendered as its `snake_case` variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WarningSeverityDoc {
    /// Informational.
    Info,
    /// Degraded but operating.
    Warning,
    /// Operator action required.
    Critical,
}

/// `OpenAPI` mirror of [`multiview_events::WarningCode`] (SA-0 catalog).
///
/// Serde-equivalent: a unit enum rendered as its `kebab-case` variant name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum WarningCodeDoc {
    /// A GPU is present but the compositor resolved a software/CPU adapter.
    GpuPresentNoVulkanAdapter,
}

/// `OpenAPI` mirror of [`multiview_events::HealthWarning`] (SA-0).
///
/// The body of `GET /api/v1/health`: an actionable health warning carrying a
/// stable `code` + a `remediation`. A round-trip test (`tests/health.rs`) pins
/// this shape to the real `multiview_events::HealthWarning` so they cannot drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HealthWarningDoc {
    /// The stable catalog code — the dedupe key the store + UI coalesce on.
    pub code: WarningCodeDoc,
    /// The severity.
    pub severity: WarningSeverityDoc,
    /// The affected subsystem (e.g. `compositor`, `decode`, `encode`, `gpu`).
    pub subsystem: String,
    /// A clear, human-readable description of the condition.
    pub message: String,
    /// The concrete remediation — what the operator must do to fix it.
    pub remediation: String,
    /// When the condition was first raised (engine monotonic nanoseconds).
    pub since: i64,
    /// Whether the condition is currently active.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Resource-body mirrors (ADR-W015): the `OpenAPI` contract for the documents
// accepted by `/api/v1/sources|outputs|overlays`. The handlers validate and
// store the real `multiview_config` types; these mirrors exist only so the
// generated document (and the generated SPA client) describes the per-kind
// fields instead of an opaque object. `tests/typed_resources.rs` pins the
// shapes together so they cannot drift.
// ---------------------------------------------------------------------------

/// `OpenAPI` mirror of `multiview_config::SourceAuth` (reference-only secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct SourceAuthDoc {
    /// A secret reference (e.g. `op://Servers/cam/credentials`), never plaintext.
    pub secret_ref: String,
}

/// `OpenAPI` mirror of `multiview_config::ColorOverride` (four color axes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct ColorOverrideDoc {
    /// Primaries axis (`auto` or an explicit primaries token).
    #[serde(default)]
    pub primaries: Option<String>,
    /// Transfer axis.
    #[serde(default)]
    pub transfer: Option<String>,
    /// Matrix axis.
    #[serde(default)]
    pub matrix: Option<String>,
    /// Range axis.
    #[serde(default)]
    pub range: Option<String>,
}

/// `OpenAPI` mirror of `multiview_config::CaptionSelector` (tagged by `mode`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptionSelectorDoc {
    /// Auto-select the first usable caption track.
    Auto,
    /// Captions explicitly disabled.
    Off,
    /// DVB teletext, addressed by page.
    TeletextPage {
        /// Teletext page number (typically `100`–`899`).
        page: u16,
    },
    /// A subtitle track by stream id or language tag.
    Track {
        /// The track identifier.
        id: String,
    },
    /// Embedded CEA-608/708 captions by field/service.
    EmbeddedCc {
        /// The caption field/service selector (e.g. `cc1`).
        field: String,
    },
    /// An external sidecar subtitle file (SRT/WebVTT).
    Sidecar {
        /// Filesystem path to the sidecar.
        path: String,
    },
}

/// `OpenAPI` mirror of `multiview_config::placement::PinVendor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PinVendorDoc {
    /// NVIDIA.
    Nvidia,
    /// Intel.
    Intel,
    /// AMD.
    Amd,
    /// Apple.
    Apple,
}

/// `OpenAPI` mirror of `multiview_config::placement::DevicePin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct DevicePinDoc {
    /// The vendor family.
    pub vendor: PinVendorDoc,
    /// The vendor's stable device handle (UUID / PCI bus id / registryID).
    pub stable_id: String,
}

/// `OpenAPI` mirror of `multiview_config::RtspOptions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct RtspOptionsDoc {
    /// Lower-transport selection (`tcp` / `udp`).
    pub transport: String,
}

/// `OpenAPI` mirror of `multiview_config::ClockFaceConfig`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClockFaceDoc {
    /// Analog face.
    #[default]
    Analog,
    /// Digital readout.
    Digital,
}

/// `OpenAPI` mirror of `multiview_config::SourceKind` (tagged by `kind`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SourceKindDoc {
    /// Built-in colour bars (`test` is a back-compat alias).
    #[serde(alias = "test")]
    Bars,
    /// A solid-colour slate.
    Solid {
        /// Fill colour as `#RRGGBB`/`#RGB` hex.
        color: String,
    },
    /// A full-frame clock.
    Clock {
        /// Analog (default) or digital face.
        #[serde(default)]
        face: ClockFaceDoc,
        /// 12-hour vs 24-hour mode (default 24-hour).
        #[serde(default)]
        twelve_hour: bool,
        /// Timezone offset from UTC in minutes (`-720..=840`).
        #[serde(default)]
        tz_offset_minutes: i32,
    },
    /// RTSP pull.
    Rtsp {
        /// Source URL.
        url: String,
        /// RTSP transport options.
        #[serde(default)]
        rtsp: Option<RtspOptionsDoc>,
    },
    /// HLS / M3U pull.
    Hls {
        /// Playlist URL.
        url: String,
    },
    /// `YouTube` live, resolved to HLS at runtime.
    Youtube {
        /// Watch/live/channel URL.
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
        /// NDI source name.
        name: String,
    },
    /// File input.
    File {
        /// Filesystem path.
        path: String,
    },
}

/// `OpenAPI` mirror of `multiview_config::WallClockUse` (ADR-0038 verb).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WallClockUseDoc {
    /// Use the detected wall-clock (rebase when Trusted). The default.
    #[default]
    Use,
    /// Discard it (reclock-to-house).
    Discard,
}

/// `OpenAPI` mirror of `multiview_config::SourceWallClock` (ADR-0038 verb).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct SourceWallClockDoc {
    /// `use` (rebase when Trusted) or `discard` (reclock-to-house).
    #[serde(rename = "use", default)]
    pub use_: WallClockUseDoc,
}

/// `OpenAPI` mirror of `multiview_config::Source` — the body accepted by
/// `POST`/`PUT /api/v1/sources/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct SourceBodyDoc {
    /// Stable input id; may be omitted (the path id is injected).
    #[serde(default)]
    pub id: Option<String>,
    /// Human-friendly display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The kind-specific payload (`kind` + its fields sit at top level).
    #[serde(flatten)]
    pub kind: SourceKindDoc,
    /// Reference-only credentials.
    #[serde(default)]
    pub auth: Option<SourceAuthDoc>,
    /// Per-source color override.
    #[serde(default)]
    pub color_override: Option<ColorOverrideDoc>,
    /// Caption/subtitle selector.
    #[serde(default)]
    pub captions: Option<CaptionSelectorDoc>,
    /// Operator decode-stage GPU pin.
    #[serde(default)]
    pub gpu_pin: Option<DevicePinDoc>,
    /// Wall-clock Use/Discard verb.
    #[serde(default)]
    pub wallclock: Option<SourceWallClockDoc>,
}

/// `OpenAPI` mirror of `multiview_config::audio::OutputAudioMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputAudioModeDoc {
    /// Carry only the mixed program bus.
    Program,
    /// Carry an explicit list of selectable tracks.
    Tracks,
}

/// `OpenAPI` mirror of `multiview_config::audio::OutputAudio`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct OutputAudioDoc {
    /// `program` (mixed bus) or `tracks` (explicit selection).
    pub mode: OutputAudioModeDoc,
    /// The selectable-track list (used only in `tracks` mode).
    #[serde(default)]
    pub tracks: Vec<String>,
}

/// `OpenAPI` mirror of `multiview_config::Output` (tagged by `kind`) — the body
/// accepted by `POST`/`PUT /api/v1/outputs/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputBodyDoc {
    /// RTSP server.
    RtspServer {
        /// Stable operator id; may be omitted (the path id is injected).
        #[serde(default)]
        id: Option<String>,
        /// Mount point (e.g. `/multiview`).
        mount: String,
        /// Video codec (`h264`, `hevc`, …).
        codec: String,
        /// Latency profile hint.
        #[serde(default)]
        latency_profile: Option<String>,
        /// Encode-stage GPU pin.
        #[serde(default)]
        gpu_pin: Option<DevicePinDoc>,
        /// Per-output audio selection.
        #[serde(default)]
        audio: Option<OutputAudioDoc>,
    },
    /// Low-latency HLS packager.
    LlHls {
        /// Stable operator id; may be omitted.
        #[serde(default)]
        id: Option<String>,
        /// Output path.
        path: String,
        /// Video codec.
        codec: String,
        /// Target part duration (ms).
        #[serde(default)]
        part_target_ms: Option<u32>,
        /// Segment duration (ms).
        #[serde(default)]
        segment_ms: Option<u32>,
        /// GOP duration (ms).
        #[serde(default)]
        gop_ms: Option<u32>,
        /// Encode-stage GPU pin.
        #[serde(default)]
        gpu_pin: Option<DevicePinDoc>,
        /// Per-output audio selection.
        #[serde(default)]
        audio: Option<OutputAudioDoc>,
    },
    /// HLS packager.
    Hls {
        /// Stable operator id; may be omitted.
        #[serde(default)]
        id: Option<String>,
        /// Output path.
        path: String,
        /// Video codec.
        codec: String,
        /// Segment duration (ms).
        #[serde(default)]
        segment_ms: Option<u32>,
        /// Encode-stage GPU pin.
        #[serde(default)]
        gpu_pin: Option<DevicePinDoc>,
        /// Per-output audio selection.
        #[serde(default)]
        audio: Option<OutputAudioDoc>,
    },
    /// NDI output.
    Ndi {
        /// Stable operator id; may be omitted.
        #[serde(default)]
        id: Option<String>,
        /// NDI source name to advertise.
        name: String,
        /// Frame-source GPU pin.
        #[serde(default)]
        gpu_pin: Option<DevicePinDoc>,
        /// Per-output audio selection.
        #[serde(default)]
        audio: Option<OutputAudioDoc>,
    },
    /// RTMP push.
    Rtmp {
        /// Stable operator id; may be omitted.
        #[serde(default)]
        id: Option<String>,
        /// Destination URL.
        url: String,
        /// Video codec.
        codec: String,
        /// Encode-stage GPU pin.
        #[serde(default)]
        gpu_pin: Option<DevicePinDoc>,
        /// Per-output audio selection.
        #[serde(default)]
        audio: Option<OutputAudioDoc>,
    },
    /// SRT push.
    Srt {
        /// Stable operator id; may be omitted.
        #[serde(default)]
        id: Option<String>,
        /// Destination URL.
        url: String,
        /// Video codec.
        codec: String,
        /// Encode-stage GPU pin.
        #[serde(default)]
        gpu_pin: Option<DevicePinDoc>,
        /// Per-output audio selection.
        #[serde(default)]
        audio: Option<OutputAudioDoc>,
    },
}

/// `OpenAPI` mirror of `multiview_config::Overlay` — the body accepted by
/// `POST`/`PUT /api/v1/overlays/{id}`.
///
/// Overlay kinds carry a large, kind-dependent parameter set captured verbatim
/// (lossless round-trip), so the schema documents the common envelope and
/// leaves the per-kind extras additive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct OverlayBodyDoc {
    /// Stable overlay id; may be omitted (the path id is injected).
    #[serde(default)]
    pub id: Option<String>,
    /// Overlay kind (`clock`, `label`, `tally_border`, `image`, `subtitle`, …).
    pub kind: String,
    /// Attachment target (`canvas` or a cell id).
    pub target: String,
    /// Stacking order.
    #[serde(default)]
    pub z: i32,
    /// Kind-specific parameters, captured verbatim.
    #[serde(flatten)]
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// The request envelope for `POST`/`PUT /api/v1/sources/{id}` (`name` + body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct SourceResourceInputDoc {
    /// Human-friendly name.
    pub name: String,
    /// The source document.
    pub body: SourceBodyDoc,
}

/// The request envelope for `POST`/`PUT /api/v1/outputs/{id}` (`name` + body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct OutputResourceInputDoc {
    /// Human-friendly name.
    pub name: String,
    /// The output document.
    pub body: OutputBodyDoc,
}

/// The request envelope for `POST`/`PUT /api/v1/overlays/{id}` (`name` + body).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[non_exhaustive]
pub struct OverlayResourceInputDoc {
    /// Human-friendly name.
    pub name: String,
    /// The overlay document.
    pub body: OverlayBodyDoc,
}
