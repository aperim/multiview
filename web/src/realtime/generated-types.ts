// GENERATED FILE — do not edit by hand.
// Source: docs/api/asyncapi.json (produced by `cargo xtask gen-asyncapi`).
// Regenerate: npm run generate:events
// Consumers: hand-authored runtime in envelope.ts and connection.ts is NOT
// replaced — see ADR-RT006. Import from this module for precise payload types.

/** Data body of `alarm.raised`, `alarm.updated`, `alarm.cleared`, `alarm.acked`. Carries the current AlarmRecord value after the transition (X.733-aligned). */
export interface AlarmTransition {
  /** The current alarm record after the transition. */
  readonly record: Record<string, unknown>;
}

/** Data body of `alert.raised` and `alert.cleared` events. The `key` field is a stable dedupe identifier: multiple raised frames with the same key coalesce. */
export interface Alert {
  /** Whether the condition is currently active. */
  readonly active: boolean;
  /** Optional longer detail. */
  readonly detail?: string;
  /** Stable dedupe key for the condition. */
  readonly key: string;
  readonly severity: AlertSeverity;
  /** Short human-readable title. */
  readonly title: string;
}

/** Severity of an operator alert. */
export type AlertSeverity = "info" | "warning" | "critical";

/** Data body of the `audio.meter` event: a high-rate per-input/track peak/RMS/clip sample. This is numeric metadata only — never audio content. Conflated/sampled at 10–30 Hz on the wire (high-rate lane). */
export interface AudioMeter {
  /** Whether any channel clipped in this window. */
  readonly clip: boolean;
  /** Whether the meter pipeline overflowed (dropped windows). */
  readonly overflow: boolean;
  /** Per-channel peak level (dBFS). */
  readonly peak_db: readonly number[];
  /** Per-channel RMS level (dBFS). */
  readonly rms_db: readonly number[];
  /** Effective wire cadence (Hz). */
  readonly sampled_hz: number;
  /** Track index. */
  readonly track: number;
}

/** Data body of `$hello`: the first server frame after auth. */
export interface Hello {
  /** Default wire cadence when `rate_hz` is omitted from `$subscribe`. */
  readonly default_rate_hz: number;
  /** Heartbeat interval (milliseconds). */
  readonly heartbeat_ms: number;
  /** Maximum clamped wire cadence (Hz). */
  readonly max_rate_hz: number;
  /** Minimum clamped wire cadence (Hz). */
  readonly min_rate_hz: number;
  /** Replay ring size (frames per session/topic). */
  readonly replay_ring: number;
  /** Envelope schema majors this server can speak. */
  readonly server_v: readonly number[];
  /** Server-assigned session id. */
  readonly session_id: string;
}

/** Data body of the `input.connection` event. */
export interface InputConnection {
  /** Reconnect attempt counter, if reconnecting. */
  readonly attempt?: number;
  readonly state: LifecycleState;
}

/** Data body of `job.progress`; correlated to a REST command via the envelope `corr`. */
export interface JobProgress {
  /** Optional human-readable progress message. */
  readonly message?: string;
  /** Percent complete (0–100). */
  readonly pct: number;
  /** Short machine-readable phase label. */
  readonly phase: string;
}

/** Data body of `$lag`: this connection's queue overflowed for a topic. */
export interface Lag {
  readonly action: LagAction;
  /** Number of frames dropped. */
  readonly dropped_n: number;
  /** Topic whose frames were dropped. */
  readonly topic: string;
}

/** What the server did with the dropped frames. */
export type LagAction = "conflated" | "resnapshot";

/** Tile/input lifecycle state (invariant #2: Live → Stale → Reconnecting → NoSignal). */
export type LifecycleState = "LIVE" | "STALE" | "RECONNECTING" | "NO_SIGNAL";

/** Running state of an output sink. */
export type OutputRunState = "starting" | "running" | "migrating" | "error";

/** Data body of the `output.status` event. */
export interface OutputStatus {
  /** Measured output bitrate (bits/sec), if known. */
  readonly bitrate_bps?: number;
  /** Number of currently-connected consumers, if known. */
  readonly clients?: number;
  readonly state: OutputRunState;
}

/** Data body of `$error`: a control-plane error. */
export interface ProtocolError {
  /** Short stable machine-readable error code. */
  readonly code: string;
  /** Human-readable description. */
  readonly message: string;
}

/** Data body of `$resume` (client→server): present last-seen cursor on reconnect. */
export interface Resume {
  /** Last `seq` the client successfully observed. */
  readonly last_seq: number;
  /** Session to resume. */
  readonly session_id: string;
}

/** Data body of `$resync`: the gap is unrecoverable; the client MUST rebuild state from the fresh snapshot that follows. */
export interface Resync {
  readonly reason: ResyncReason;
  /** Topics the client must rebuild. */
  readonly resubscribe: readonly string[];
}

/** Why a $resync was issued. */
export type ResyncReason = "seq_evicted" | "unknown_session" | "session_expired";

/** Data body of `salvo.armed`, `salvo.taken`, `salvo.cancelled`. */
export interface SalvoEvent {
  /** Output head this recall applies to, if scoped. */
  readonly head?: string;
  /** The lifecycle phase this event reports. */
  readonly phase: "armed" | "taken" | "cancelled";
  /** Stable salvo identifier/name. */
  readonly salvo: string;
}

/** Data body of `$set_rate` (client→server): change a topic's wire cadence. */
export interface SetRate {
  /** Requested cadence (Hz); server clamps to [min, max]. */
  readonly rate_hz: number;
  /** Topic whose cadence is changing. */
  readonly topic: string;
}

/** Data body of `$subscribe` (client→server): subscribe to topics. */
export interface Subscribe {
  /** Optional resource-id allowlist. */
  readonly ids?: readonly string[];
  /** Optional max cadence (Hz); server clamps and reports effective. */
  readonly rate_hz?: number;
  /** Optional resume cursor: subscribe + replay from after this seq. */
  readonly since_seq?: number;
  /** Topics to subscribe to. */
  readonly topics: readonly string[];
}

/** Data body of `$subscribed`: per-topic ack before the snapshot. */
export interface Subscribed {
  /** Actual cadence after server clamping. */
  readonly effective_rate_hz: number;
  /** The `seq` the forthcoming snapshot is current as of. */
  readonly snapshot_seq: number;
  /** The topic that was subscribed. */
  readonly topic: string;
}

/** Data body of `tally.state`: resolved tally lamp/UMD state for one element. */
export interface TallyEvent {
  /** The resolved tally lamp/UMD state. */
  readonly state: Record<string, unknown>;
  readonly target: TallyTarget;
}

/** What a tally state applies to (tagged by `kind`). */
export type TallyTarget =
  | {
  /** Zero-based tile index. */
  readonly index: number;
  readonly kind: "tile";
}
  | {
  readonly kind: "element";
  /** Element name. */
  readonly name: string;
};

/** Data body of the `tile.state` event: a tile lifecycle transition. */
export interface TileState {
  readonly from: LifecycleState;
  /** The input bound to the tile at the time of the transition, if any. */
  readonly input?: string;
  readonly to: LifecycleState;
  /** Short machine-readable trigger label (e.g. `nosignal_timeout`). */
  readonly trigger: string;
}

/** Data body of `$unsubscribe` (client→server): stop receiving topics. */
export interface Unsubscribe {
  /** Topics to stop receiving. */
  readonly topics: readonly string[];
}
