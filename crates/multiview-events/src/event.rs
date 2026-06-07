//! The discriminated event payload — one internally-tagged union for every
//! frame in both directions.
//!
//! Per ADR-RT002 the payload is a Rust internally-tagged enum
//! (`#[serde(tag = "t", content = "data")]`) that renders as a JSON-Schema
//! `oneOf` with a `const` discriminator and maps to a perfect TypeScript
//! discriminated `switch`. Control frames (`$`-prefixed `t`) and data events
//! (dotted `t`) share the one union so a single parse/route path serves WS and
//! SSE.
//!
//! This is **not** `#[serde(untagged)]` (guardrails + conventions §5 forbid it);
//! the explicit `t` tag gives one unambiguous parse path.
use multiview_core::alarm::AlarmRecord;
use multiview_core::tally::TallyState;
use multiview_core::traits::SourceState;
use serde::{Deserialize, Serialize};

use crate::subscription::{
    Hello, Lag, ProtocolError, Resume, Resync, SetRate, Subscribe, Subscribed, Unsubscribe,
};

/// The wire form of a tile/source lifecycle state (invariant #2:
/// `Live -> Stale -> Reconnecting -> NoSignal`).
///
/// This is the serde-serializable mirror of [`multiview_core::traits::SourceState`]
/// (which is `#[non_exhaustive]` and intentionally carries no serde impl). It
/// owns the wire contract for tile/input state — uppercase strings as in the
/// realtime-api brief — and converts losslessly from the core enum via
/// [`From<SourceState>`](LifecycleState::from). Any future `SourceState`
/// variant the conversion does not know maps to [`LifecycleState::NoSignal`]
/// (the safe "render a placeholder" default) rather than panicking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum LifecycleState {
    /// Delivering fresh frames.
    Live,
    /// Holding the last-good frame; no fresh frame recently.
    Stale,
    /// The supervisor is re-establishing the connection.
    Reconnecting,
    /// No usable signal; a placeholder is rendered.
    NoSignal,
}

impl From<SourceState> for LifecycleState {
    fn from(state: SourceState) -> Self {
        match state {
            SourceState::Live => Self::Live,
            SourceState::Stale => Self::Stale,
            SourceState::Reconnecting => Self::Reconnecting,
            // `SourceState` is `#[non_exhaustive]`; an unknown future variant
            // maps to the safe placeholder state.
            _ => Self::NoSignal,
        }
    }
}

/// The trigger that drove a tile state transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileState {
    /// The state the tile transitioned from.
    pub from: LifecycleState,
    /// The state the tile transitioned to.
    pub to: LifecycleState,
    /// The input bound to the tile, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// A short machine-readable trigger label (e.g. `nosignal_timeout`).
    pub trigger: String,
}

/// A high-rate audio meter sample (numeric only — never audio).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioMeter {
    /// The track index this sample is for.
    pub track: u32,
    /// Per-channel peak level (dBFS).
    pub peak_db: Vec<f32>,
    /// Per-channel RMS level (dBFS).
    pub rms_db: Vec<f32>,
    /// Whether any channel clipped in this window.
    pub clip: bool,
    /// Whether the meter pipeline overflowed (dropped windows).
    pub overflow: bool,
    /// The effective sampling cadence on the wire (Hz).
    pub sampled_hz: u32,
}

/// The hardware vendor of a GPU/accelerator (for telemetry display + which
/// per-engine signals to expect). `#[non_exhaustive]` so a future vendor does
/// not break the wire enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum GpuVendor {
    /// NVIDIA — exposes compute, encoder/decoder util, and NVENC session counts.
    Nvidia,
    /// Intel — exposes compute + encoder/decoder util.
    Intel,
    /// AMD — exposes compute + a combined media-engine util.
    Amd,
    /// Apple — exposes compute; no per-engine util.
    Apple,
    /// A vendor the control plane did not classify.
    Other,
}

/// A high-rate per-GPU utilisation sample (numeric only). Optional fields are
/// absent where the vendor does not expose that signal (see [`GpuVendor`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuMetrics {
    /// Stable device identity (UUID where available, else an index string).
    pub id: String,
    /// The hardware vendor.
    pub vendor: GpuVendor,
    /// Human-readable device name, if known (e.g. `NVIDIA GeForce RTX 4060`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Compute (graphics/CUDA) utilisation, 0.0–1.0.
    pub compute_util: f32,
    /// VRAM in use (bytes).
    pub mem_used_bytes: u64,
    /// Total VRAM (bytes).
    pub mem_total_bytes: u64,
    /// Encoder (NVENC/QSV) ASIC utilisation, 0.0–1.0, where the vendor exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_util: Option<f32>,
    /// Decoder (NVDEC/QSV) ASIC utilisation, 0.0–1.0, where the vendor exposes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decoder_util: Option<f32>,
    /// Active concurrent hardware encode sessions (NVIDIA), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_sessions: Option<u32>,
    /// The runtime-discovered concurrent encode-session ceiling (NVIDIA), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder_session_ceiling: Option<u32>,
}

/// A high-rate whole-system metrics sample (numeric only — never a screenshot).
/// Pushed on topic [`crate::topic::Topic::System`], **conflated + drop-oldest**
/// like [`AudioMeter`] (inv #10): the engine never blocks on a client, and a slow
/// UI simply skips samples. The UI subscribes — it never polls these (a REST poll
/// of a per-second value is the wrong shape; only cold historic windows are REST).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemMetrics {
    /// Whole-system CPU utilisation, 0.0–1.0 (from `/proc/stat` on Linux).
    pub cpu_util: f32,
    /// Host memory in use (bytes), where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_bytes: Option<u64>,
    /// Total host memory (bytes), where known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_bytes: Option<u64>,
    /// Per-GPU utilisation samples; empty on a GPU-free host.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpus: Vec<GpuMetrics>,
    /// Aggregate program output rate across active programs (fps), if running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program_fps: Option<f32>,
    /// The effective sampling cadence on the wire (Hz).
    pub sampled_hz: u32,
}

/// The running state of an output sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OutputRunState {
    /// Not running — program output is stopped/idle.
    Idle,
    /// Coming up.
    Starting,
    /// Serving program output.
    Running,
    /// Make-before-break migration in progress.
    Migrating,
    /// Faulted.
    Error,
}

/// An output sink status update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputStatus {
    /// The run state.
    pub state: OutputRunState,
    /// Measured output bitrate (bits/sec), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitrate_bps: Option<u64>,
    /// Number of currently-connected consumers, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clients: Option<u32>,
}

/// Severity of an operator alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AlertSeverity {
    /// Informational.
    Info,
    /// Degraded but operating.
    Warning,
    /// Operator action required.
    Critical,
}

/// An operator alert raised, updated, or cleared. The `key` dedupes the same
/// condition so it coalesces rather than spamming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    /// Stable dedupe key for the condition.
    pub key: String,
    /// The severity.
    pub severity: AlertSeverity,
    /// A short human-readable title.
    pub title: String,
    /// Optional longer detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Whether the condition is currently active.
    pub active: bool,
}

/// An input source connection-state change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputConnection {
    /// The new lifecycle state of the source.
    pub state: LifecycleState,
    /// The reconnect attempt counter, if reconnecting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// Progress of a long-running REST command job (correlated via `corr`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobProgress {
    /// A short machine-readable phase label.
    pub phase: String,
    /// Percent complete, `0..=100`.
    pub pct: u8,
    /// An optional human-readable progress message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// An alarm lifecycle transition carrying the full X.733 alarm record.
///
/// This is the `data` body of the `alarm.raised` / `alarm.updated` /
/// `alarm.cleared` / `alarm.acked` events (topic [`crate::topic::Topic::Alarms`]).
/// The kind of transition is encoded by the surrounding [`Event`] variant; the
/// body is the same [`AlarmRecord`] in every case so a receiver always has the
/// current value of the alarm (its severity, scope, dwell and ack state). The
/// roll-up/state-machine logic that produces these records lives in
/// `multiview-engine`; this crate only carries them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlarmTransition {
    /// The current value of the alarm after the transition.
    pub record: AlarmRecord,
}

impl AlarmTransition {
    /// Wrap an [`AlarmRecord`] as an alarm-transition event body.
    #[must_use]
    pub const fn new(record: AlarmRecord) -> Self {
        Self { record }
    }
}

/// What a tally state applies to — the routing scope of a [`TallyEvent`].
///
/// Serialised **tagged** (`#[serde(tag = "kind")]`) per repo conventions; never
/// `untagged`. `#[non_exhaustive]` so finer targets can be added later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum TallyTarget {
    /// A multiview tile, by zero-based index.
    Tile {
        /// Zero-based tile index.
        index: u32,
    },
    /// A named UMD/tally element (e.g. an external tally-bus name).
    Element {
        /// Element name.
        name: String,
    },
}

/// A resolved tally state for one element (topic [`crate::topic::Topic::Tally`]).
///
/// Carries the [`TallyState`] (lamp colour, brightness, originating bus)
/// produced by the tally arbiter in `multiview-engine` and the
/// [`target`](TallyEvent::target) it applies to. Rendered by `multiview-overlay`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TallyEvent {
    /// What the tally state applies to.
    pub target: TallyTarget,
    /// The resolved tally lamp/UMD state.
    pub state: TallyState,
}

/// The lifecycle phase a salvo transitioned into.
///
/// A salvo is a named atomic multi-element recall (layout + source assignment +
/// UMD + tally arming) applied via an arm/take (ADR-MV004). Serialised
/// **tagged** (`#[serde(tag = "phase")]`); never `untagged`. `#[non_exhaustive]`
/// so additional phases can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
#[non_exhaustive]
pub enum SalvoPhase {
    /// The salvo has been armed (staged) and is awaiting a take.
    Armed,
    /// The salvo has been taken (applied atomically).
    Taken,
    /// A previously-armed salvo was cancelled before being taken.
    Cancelled,
}

/// A salvo arm/take lifecycle event (topic [`crate::topic::Topic::Tally`]).
///
/// Identifies the [`salvo`](SalvoEvent::salvo) by name, the
/// [`phase`](SalvoEvent::phase) it transitioned into, and optionally the output
/// [`head`](SalvoEvent::head) the recall applies to (multi-head walls).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SalvoEvent {
    /// Stable salvo identifier/name.
    pub salvo: String,
    /// The lifecycle phase this event reports.
    pub phase: SalvoPhase,
    /// The output head this recall applies to, if scoped to one head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl SalvoEvent {
    /// Construct a salvo event for `salvo` entering `phase`, with no head scope.
    #[must_use]
    pub fn new(salvo: impl Into<String>, phase: SalvoPhase) -> Self {
        Self {
            salvo: salvo.into(),
            phase,
            head: None,
        }
    }

    /// Builder: scope this salvo event to a specific output head.
    #[must_use]
    pub fn with_head(mut self, head: impl Into<String>) -> Self {
        self.head = Some(head.into());
        self
    }
}

/// The discriminated payload of every frame: control frames and data events.
///
/// Internally tagged on `t` with the body under `data` (ADR-RT002). The
/// `Envelope` flattens this so the wire frame carries `t` and `data` alongside
/// the envelope metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "data")]
#[non_exhaustive]
pub enum Event {
    // ---- Control frames (topic == `$control`) ----
    /// First server frame after auth.
    #[serde(rename = "$hello")]
    Hello(Hello),
    /// Client asks to receive topics.
    #[serde(rename = "$subscribe")]
    Subscribe(Subscribe),
    /// Per-topic subscribe ack (precedes the snapshot).
    #[serde(rename = "$subscribed")]
    Subscribed(Subscribed),
    /// Client stops receiving topics.
    #[serde(rename = "$unsubscribe")]
    Unsubscribe(Unsubscribe),
    /// Client changes a high-rate topic's wire cadence.
    #[serde(rename = "$set_rate")]
    SetRate(SetRate),
    /// Client presents its resume cursor after reconnect.
    #[serde(rename = "$resume")]
    Resume(Resume),
    /// Server: gap unrecoverable; client must rebuild from a fresh snapshot.
    #[serde(rename = "$resync")]
    Resync(Resync),
    /// Server: this connection overflowed; re-snapshot the affected topic.
    #[serde(rename = "$lag")]
    Lag(Lag),
    /// Application-level heartbeat (server->client).
    #[serde(rename = "$ping")]
    Ping,
    /// Application-level heartbeat reply (client->server).
    #[serde(rename = "$pong")]
    Pong,
    /// A control-plane error.
    #[serde(rename = "$error")]
    Error(ProtocolError),

    // ---- Data events (dotted `t`, carried on their topic) ----
    /// Tile state-machine transition (topic `tiles`).
    #[serde(rename = "tile.state")]
    TileState(TileState),
    /// High-rate audio meter sample (topic `audio.meters`).
    #[serde(rename = "audio.meter")]
    AudioMeter(AudioMeter),
    /// High-rate whole-system metrics sample (topic `system`): cpu / gpu /
    /// encoder-decoder. Conflated + drop-oldest; pushed, never polled.
    #[serde(rename = "system.metrics")]
    SystemMetrics(SystemMetrics),
    /// Output sink status (topic `outputs`).
    #[serde(rename = "output.status")]
    OutputStatus(OutputStatus),
    /// An operator alert raised (topic `alerts`).
    #[serde(rename = "alert.raised")]
    AlertRaised(Alert),
    /// An operator alert cleared (topic `alerts`).
    #[serde(rename = "alert.cleared")]
    AlertCleared(Alert),
    /// Input source connection change (topic `inputs`).
    #[serde(rename = "input.connection")]
    InputConnection(InputConnection),
    /// Long-running job progress (topic `jobs`, correlated by `corr`).
    #[serde(rename = "job.progress")]
    JobProgress(JobProgress),

    // ---- Broadcast monitoring/control events ----
    /// A monitoring alarm was first raised (topic `alarms`).
    #[serde(rename = "alarm.raised")]
    AlarmRaised(AlarmTransition),
    /// An active alarm's value changed — e.g. severity escalated or dwell
    /// advanced (topic `alarms`).
    #[serde(rename = "alarm.updated")]
    AlarmUpdated(AlarmTransition),
    /// An alarm's underlying condition cleared (topic `alarms`).
    #[serde(rename = "alarm.cleared")]
    AlarmCleared(AlarmTransition),
    /// An operator acknowledged an alarm (topic `alarms`).
    #[serde(rename = "alarm.acked")]
    AlarmAcked(AlarmTransition),
    /// A tile/element's resolved tally lamp/UMD state changed (topic `tally`).
    #[serde(rename = "tally.state")]
    TallyState(TallyEvent),
    /// A salvo was armed (staged, awaiting take) (topic `tally`).
    #[serde(rename = "salvo.armed")]
    SalvoArmed(SalvoEvent),
    /// A salvo was taken (applied atomically) (topic `tally`).
    #[serde(rename = "salvo.taken")]
    SalvoTaken(SalvoEvent),
    /// A previously-armed salvo was cancelled before being taken (topic `tally`).
    #[serde(rename = "salvo.cancelled")]
    SalvoCancelled(SalvoEvent),
}

impl Event {
    /// The wire discriminator string (`t`) for this event.
    #[must_use]
    pub const fn type_tag(&self) -> &'static str {
        match self {
            Self::Hello(_) => "$hello",
            Self::Subscribe(_) => "$subscribe",
            Self::Subscribed(_) => "$subscribed",
            Self::Unsubscribe(_) => "$unsubscribe",
            Self::SetRate(_) => "$set_rate",
            Self::Resume(_) => "$resume",
            Self::Resync(_) => "$resync",
            Self::Lag(_) => "$lag",
            Self::Ping => "$ping",
            Self::Pong => "$pong",
            Self::Error(_) => "$error",
            Self::TileState(_) => "tile.state",
            Self::AudioMeter(_) => "audio.meter",
            Self::SystemMetrics(_) => "system.metrics",
            Self::OutputStatus(_) => "output.status",
            Self::AlertRaised(_) => "alert.raised",
            Self::AlertCleared(_) => "alert.cleared",
            Self::InputConnection(_) => "input.connection",
            Self::JobProgress(_) => "job.progress",
            Self::AlarmRaised(_) => "alarm.raised",
            Self::AlarmUpdated(_) => "alarm.updated",
            Self::AlarmCleared(_) => "alarm.cleared",
            Self::AlarmAcked(_) => "alarm.acked",
            Self::TallyState(_) => "tally.state",
            Self::SalvoArmed(_) => "salvo.armed",
            Self::SalvoTaken(_) => "salvo.taken",
            Self::SalvoCancelled(_) => "salvo.cancelled",
        }
    }

    /// Whether this is a control frame (carried on [`crate::topic::Topic::Control`]).
    #[must_use]
    pub const fn is_control(&self) -> bool {
        matches!(
            self,
            Self::Hello(_)
                | Self::Subscribe(_)
                | Self::Subscribed(_)
                | Self::Unsubscribe(_)
                | Self::SetRate(_)
                | Self::Resume(_)
                | Self::Resync(_)
                | Self::Lag(_)
                | Self::Ping
                | Self::Pong
                | Self::Error(_)
        )
    }
}
