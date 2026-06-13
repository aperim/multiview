// Typed HTTP bindings + projections for the ephemeral cast sessions
// (DEV-D3 over the DEV-D2 surface, ADR-M011).
//
// Sessions are runtime-only records under `/api/v1/cast/sessions` — never
// part of the devices resource store, so a config export can never emit one;
// `POST /{id}/save` promotes one into a normal `Device{driver: cast}`
// registry entry (which DOES export). Request/response shapes reuse the
// generated `components['schemas']` types from ../api/schema — never
// hand-written duplicates — through the shared helpers in ../api/operations
// (the ../devices/api idiom). Each served doc carries its live lifecycle
// state from the latest-wins status registry; the conflated `device.status`
// realtime lane (keyed by the session id) stays primary for freshness.
import type { components } from '../api/schema';
import {
  apiUrl,
  buildHeaders,
  OperationApiError,
  readProblem,
} from '../api/operations';
import type { RequestOptions } from '../api/operations';
import type { ResourceRecord } from '../resources/types';
import type { DeviceState } from '../realtime/generated-types';

/** The wire doc for one ephemeral session, as the spec models it. */
type CastSessionDoc = components['schemas']['CastSessionDoc'];

/** The start-session request body, as the spec models it. */
export type StartCastSessionRequest =
  components['schemas']['StartCastSessionRequest'];

/** The save-as-device request body, as the spec models it. */
export type SaveCastSessionRequest =
  components['schemas']['SaveCastSessionRequest'];

/** One ephemeral cast session as the UI consumes it. */
export interface CastSessionView {
  /** The runtime session id (`cast-session-…`, UUID-fresh per start). */
  readonly id: string;
  /** The operator-facing name, when one was given. */
  readonly name: string | undefined;
  /** The device authority dialled (`host[:port]`, IPv6 bracketed). */
  readonly address: string;
  /** The output id whose rendition the session casts. */
  readonly output: string;
  /** The resolved device-reachable media URL the session LOADs. */
  readonly mediaUrl: string;
  /** The live lifecycle state token (DEV-A3 wire vocabulary). */
  readonly state: string;
  /**
   * When the receiver accepted the session's `LOAD` (the first `MEDIA_STATUS`
   * attributing an active media session to the actor — the moment the cast
   * verifiably began showing), as Unix-epoch wall nanoseconds. `undefined`
   * until then: a session whose LOAD was refused, or is still establishing,
   * has not started (DEV-D3.1). Unlike the engine-monotonic `last_seen_ts`,
   * this is wall time and ages directly against `Date.now()`.
   */
  readonly startedUnixNs: number | undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** A session doc from the wire, validated structurally (no `as`-cast). */
function isCastSessionDoc(value: unknown): value is CastSessionDoc {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.address === 'string' &&
    typeof value.output === 'string' &&
    typeof value.media_url === 'string' &&
    typeof value.state === 'string' &&
    (value.name === undefined || value.name === null || typeof value.name === 'string')
  );
}

/** Project a wire session doc onto the {@link CastSessionView}. */
export function toCastSessionView(doc: CastSessionDoc): CastSessionView {
  return {
    id: doc.id,
    name: typeof doc.name === 'string' ? doc.name : undefined,
    address: doc.address,
    output: doc.output,
    mediaUrl: doc.media_url,
    state: doc.state,
    startedUnixNs:
      typeof doc.started_unix_ns === 'number' && Number.isFinite(doc.started_unix_ns)
        ? doc.started_unix_ns
        : undefined,
  };
}

/** The DEV-A3 lifecycle vocabulary (`DeviceState` from the AsyncAPI spec). */
const DEVICE_STATES: readonly DeviceState[] = [
  'DISCOVERED',
  'ADOPTING',
  'ONLINE',
  'DEGRADED',
  'AUTH_FAILED',
  'UNREACHABLE',
];

/**
 * Fold a wire state token onto the typed {@link DeviceState}, or `undefined`
 * for an unrecognized token — a state is displayed raw then, never invented.
 */
export function asDeviceState(token: string): DeviceState | undefined {
  return DEVICE_STATES.find((state) => state === token);
}

/**
 * The operator-facing message for a failed cast call. RFC 9457 problem
 * titles are generic ("Conflict with current state"); the `detail` carries
 * the actionable why (e.g. "no castable HLS rendition: set
 * control.cast_media_base"), so it wins when present.
 */
export function operationErrorMessage(error: unknown): string {
  if (error instanceof OperationApiError && error.detail !== undefined) {
    return error.detail;
  }
  return error instanceof Error ? error.message : String(error);
}

const SESSIONS_PATH = '/api/v1/cast/sessions';

/**
 * `GET /api/v1/cast/sessions` — the live ephemeral sessions, id-sorted, each
 * carrying its live lifecycle state. Malformed rows are dropped, never
 * fabricated (the listDiscovered idiom).
 */
export async function listCastSessions(
  options: RequestOptions = {},
): Promise<CastSessionView[]> {
  const response = await fetch(apiUrl(options, SESSIONS_PATH), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body)) {
    return [];
  }
  const sessions: CastSessionView[] = [];
  for (const raw of body) {
    if (isCastSessionDoc(raw)) {
      sessions.push(toCastSessionView(raw));
    }
  }
  return sessions;
}

/**
 * `POST /api/v1/cast/sessions` — start an ad-hoc session (201). The actor
 * CONNECTs → LAUNCHes the Default Media Receiver → LOADs the rendition; a
 * `409` means no castable rendition (or no live cast driver in this build)
 * and a `422` names the bad address/output — both surface via
 * {@link operationErrorMessage}.
 */
export async function startCastSession(
  request: StartCastSessionRequest,
  options: RequestOptions = {},
): Promise<CastSessionView> {
  const response = await fetch(apiUrl(options, SESSIONS_PATH), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(request),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isCastSessionDoc(body)) {
    throw new OperationApiError('The server returned an unexpected cast session body.');
  }
  return toCastSessionView(body);
}

/**
 * `DELETE /api/v1/cast/sessions/{id}` — stop a session (the receiver STOP
 * that actually clears the TV). A `404` is idempotent success: the session
 * is already gone.
 */
export async function stopCastSession(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(
    apiUrl(options, `${SESSIONS_PATH}/${encodeURIComponent(id)}`),
    { method: 'DELETE', headers: buildHeaders(options, false) },
  );
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

/**
 * `POST /api/v1/cast/sessions/{id}/save` — promote the session to a normal
 * `Device{driver: cast}` registry entry (201). The promoted device exports
 * with the configuration and the TV keeps playing across the promotion (the
 * ephemeral actor is retired without a receiver STOP). A `409` means the
 * device id already exists.
 */
export async function saveCastSession(
  id: string,
  request: SaveCastSessionRequest,
  options: RequestOptions = {},
): Promise<ResourceRecord> {
  const response = await fetch(
    apiUrl(options, `${SESSIONS_PATH}/${encodeURIComponent(id)}/save`),
    {
      method: 'POST',
      headers: buildHeaders(options, true),
      body: JSON.stringify(request),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (
    !isRecord(body) ||
    typeof body.id !== 'string' ||
    typeof body.name !== 'string' ||
    !isRecord(body.body)
  ) {
    throw new OperationApiError('The server returned an unexpected device body.');
  }
  return { id: body.id, name: body.name, body: body.body };
}
