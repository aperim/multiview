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
