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
