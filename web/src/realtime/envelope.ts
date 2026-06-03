// Realtime envelope types (docs/api/realtime.md §2).
//
// TODO(api-schema): these wire types are defined in the `mosaic-events` crate
// and surface in the AsyncAPI document (`/asyncapi.json`), NOT yet in the
// OpenAPI schema this app generates its client from (`src/api/schema.ts`). Until
// `mosaic-events` types are exported into a generated TS module, this is a
// hand-modelled view of the documented contract — deliberately marked so it is
// replaced by codegen, never silently trusted. Parsing is defensive: unknown
// `t` values and unknown envelope majors are tolerated, never thrown on.

/** The envelope schema major this client speaks. Reject unknown majors. */
export const ENVELOPE_MAJOR = 1;

/** Control frames travel on this synthetic topic. */
export const CONTROL_TOPIC = "$control";

/**
 * The versioned envelope wrapping every realtime message. `data` is kept as
 * `unknown` here and narrowed per `t` by the event reducers; this keeps the
 * transport layer agnostic to the (large, evolving) payload union.
 */
export interface Envelope {
  /** Envelope schema major. Clients reject an unknown major. */
  readonly v: number;
  /** Dotted event type; the discriminator selecting the `data` schema. */
  readonly t: string;
  /** Subscription routing key (control frames use `$control`). */
  readonly topic: string;
  /** Optional resource scope (tile/input/output/job id). */
  readonly id?: string;
  /** Per-connection monotonic resume cursor; a gap means drops. */
  readonly seq: number;
  /** Engine monotonic nanoseconds. */
  readonly ts: number;
  /** Optional correlation id echoing a REST command / job. */
  readonly corr?: string;
  /** Typed payload selected by `t` (narrowed by reducers). */
  readonly data: unknown;
}

/** Narrow an unknown value to a plain record without an unsafe assertion. */
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** A coarse type guard validating the structural envelope shape. */
export function isEnvelope(value: unknown): value is Envelope {
  if (!isRecord(value)) {
    return false;
  }
  return (
    typeof value.v === "number" &&
    typeof value.t === "string" &&
    typeof value.topic === "string" &&
    typeof value.seq === "number" &&
    typeof value.ts === "number" &&
    "data" in value
  );
}

/** Parse a raw WS text frame into an Envelope, or `undefined` if malformed. */
export function parseEnvelope(raw: string): Envelope | undefined {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return undefined;
  }
  return isEnvelope(parsed) ? parsed : undefined;
}

// --- Narrowed payloads we surface today (extend as topics are wired). -------

/** The tile lifecycle state machine (resilience invariant #2). */
export type TileState =
  | "LIVE"
  | "STALE"
  | "RECONNECTING"
  | "NO_SIGNAL";

/** A tile row as carried in a `tiles` `$snapshot`. */
export interface TileSnapshotEntry {
  readonly id: string;
  readonly state: TileState;
  readonly input?: string;
  readonly fps?: number;
  readonly since_ts?: number;
  readonly reason?: string;
}

/** The `data` of a `tiles` `$snapshot`. */
export interface TilesSnapshotData {
  readonly as_of_seq: number;
  readonly tiles: readonly TileSnapshotEntry[];
}

/** The `data` of a `tile.state` delta. */
export interface TileStateDeltaData {
  readonly from: TileState;
  readonly to: TileState;
  readonly input?: string;
  readonly trigger?: string;
  readonly showing?: string;
  readonly since_ts?: number;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return isRecord(value) ? value : undefined;
}

function isTileState(value: unknown): value is TileState {
  return (
    value === "LIVE" ||
    value === "STALE" ||
    value === "RECONNECTING" ||
    value === "NO_SIGNAL"
  );
}

function parseTileEntry(value: unknown): TileSnapshotEntry | undefined {
  const record = asRecord(value);
  if (record === undefined) {
    return undefined;
  }
  const id = record.id;
  const state = record.state;
  if (typeof id !== "string" || !isTileState(state)) {
    return undefined;
  }
  const entry: {
    id: string;
    state: TileState;
    input?: string;
    fps?: number;
    since_ts?: number;
    reason?: string;
  } = { id, state };
  if (typeof record.input === "string") {
    entry.input = record.input;
  }
  if (typeof record.fps === "number") {
    entry.fps = record.fps;
  }
  if (typeof record.since_ts === "number") {
    entry.since_ts = record.since_ts;
  }
  if (typeof record.reason === "string") {
    entry.reason = record.reason;
  }
  return entry;
}

/** Narrow an envelope `data` to a tiles snapshot payload. */
export function parseTilesSnapshot(
  data: unknown,
): TilesSnapshotData | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  const rawTiles = record.tiles;
  if (!Array.isArray(rawTiles)) {
    return undefined;
  }
  const tiles: TileSnapshotEntry[] = [];
  for (const raw of rawTiles) {
    const entry = parseTileEntry(raw);
    if (entry !== undefined) {
      tiles.push(entry);
    }
  }
  const asOf = record.as_of_seq;
  return {
    as_of_seq: typeof asOf === "number" ? asOf : 0,
    tiles,
  };
}

/** Narrow an envelope `data` to a tile.state delta payload. */
export function parseTileStateDelta(
  data: unknown,
): TileStateDeltaData | undefined {
  const record = asRecord(data);
  if (record === undefined) {
    return undefined;
  }
  const from = record.from;
  const to = record.to;
  if (!isTileState(from) || !isTileState(to)) {
    return undefined;
  }
  const delta: {
    from: TileState;
    to: TileState;
    input?: string;
    trigger?: string;
    showing?: string;
    since_ts?: number;
  } = { from, to };
  if (typeof record.input === "string") {
    delta.input = record.input;
  }
  if (typeof record.trigger === "string") {
    delta.trigger = record.trigger;
  }
  if (typeof record.showing === "string") {
    delta.showing = record.showing;
  }
  if (typeof record.since_ts === "number") {
    delta.since_ts = record.since_ts;
  }
  return delta;
}
