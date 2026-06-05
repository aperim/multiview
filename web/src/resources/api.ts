// CRUD bindings for the Sources / Outputs / Overlays control-plane resources.
//
// Each resource is a `{ id, name, body }` record (`body` is the opaque, validated
// config document). The collection lives at `/api/v1/{kind}` and an item at
// `/api/v1/{kind}/{id}`, with `ETag`/`If-Match` optimistic concurrency on update
// and delete (conventions §6, RFC 9457 on conflict) — the same shape as layouts.
//
// These operations are not (yet) modelled in the generated `paths` of
// `../api/schema.ts`, so — exactly as `../api/layouts.ts` does for the layout
// write ops — the calls go through `fetch` with EXPLICITLY-TYPED request/response
// shapes and typed field guards. The opaque `body` is read with `stringField`/
// `numberField`/`boolField`, never an `as`-cast of an untyped value.
import type {
  OutputKind,
  OutputView,
  OverlayKind,
  OverlayView,
  ResourceInput,
  ResourceKind,
  ResourceRecord,
  SourceKind,
  SourceView,
} from './types';
import { OUTPUT_KINDS, OVERLAY_KINDS, SOURCE_KINDS } from './types';

/** A failed resource call, normalized to a message + status. */
export class ResourceApiError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'ResourceApiError';
    this.status = status;
  }
}

/** Connection options shared by every resource call. */
export interface ResourceRequestOptions {
  /** Base URL (defaults to same-origin, matching the typed client). */
  readonly baseUrl?: string;
  /** Optional bearer token for the `Authorization` header. */
  readonly token?: string;
  /**
   * The current ETag for `If-Match` on update/delete (optimistic concurrency).
   * Omit on create.
   */
  readonly etag?: string;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

/** Read a string field from an opaque body, or `undefined` when absent/typed wrong. */
export function stringField(body: Record<string, unknown>, key: string): string | undefined {
  const value = body[key];
  return typeof value === 'string' ? value : undefined;
}

/** Read a finite number field from an opaque body, or `undefined` otherwise. */
export function numberField(body: Record<string, unknown>, key: string): number | undefined {
  const value = body[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/** Read a boolean field from an opaque body, or `undefined` otherwise. */
export function boolField(body: Record<string, unknown>, key: string): boolean | undefined {
  const value = body[key];
  return typeof value === 'boolean' ? value : undefined;
}

function isProblem(value: unknown): value is { title: string; status: number } {
  return (
    isRecord(value) &&
    typeof value.status === 'number' &&
    typeof value.title === 'string'
  );
}

/** A resource record from the wire, validated structurally (no `as`-cast). */
function isResourceRecord(value: unknown): value is ResourceRecord {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.name === 'string' &&
    isRecord(value.body)
  );
}

async function readProblem(response: Response): Promise<ResourceApiError> {
  try {
    const body: unknown = await response.json();
    if (isProblem(body)) {
      return new ResourceApiError(body.title, body.status);
    }
  } catch {
    // Fall through to a status-only error when the body is absent/unparseable.
  }
  return new ResourceApiError(`Request failed (${String(response.status)})`, response.status);
}

function headersFor(options: ResourceRequestOptions, withJsonBody: boolean): Headers {
  const headers = new Headers();
  if (withJsonBody) {
    headers.set('Content-Type', 'application/json');
  }
  if (options.token !== undefined && options.token !== '') {
    headers.set('Authorization', `Bearer ${options.token}`);
  }
  if (options.etag !== undefined && options.etag !== '') {
    headers.set('If-Match', options.etag);
  }
  return headers;
}

function collectionUrl(kind: ResourceKind, options: ResourceRequestOptions): string {
  return `${options.baseUrl ?? ''}/api/v1/${kind}`;
}

function itemUrl(kind: ResourceKind, id: string, options: ResourceRequestOptions): string {
  return `${collectionUrl(kind, options)}/${encodeURIComponent(id)}`;
}

/** A single record plus the ETag the server stamped it with, when present. */
export interface ResourceWithEtag {
  /** The stored record. */
  readonly record: ResourceRecord;
  /** The ETag from the response, when one was carried. */
  readonly etag: string | undefined;
}

/** List a resource collection (`GET /api/v1/{kind}`), id-sorted by the server. */
export async function listResources(
  kind: ResourceKind,
  options: ResourceRequestOptions = {},
): Promise<ResourceRecord[]> {
  const response = await fetch(collectionUrl(kind, options), {
    method: 'GET',
    headers: headersFor(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isResourceRecord)) {
    throw new ResourceApiError('The server returned an unexpected resource list.');
  }
  return body;
}

/** Fetch one resource (`GET /api/v1/{kind}/{id}`) with its ETag for a later update. */
export async function getResource(
  kind: ResourceKind,
  id: string,
  options: ResourceRequestOptions = {},
): Promise<ResourceWithEtag> {
  const response = await fetch(itemUrl(kind, id, options), {
    method: 'GET',
    headers: headersFor(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isResourceRecord(body)) {
    throw new ResourceApiError('The server returned an unexpected resource body.');
  }
  const etag = response.headers.get('ETag');
  return { record: body, etag: etag ?? undefined };
}

/**
 * Create (`POST`) or update (`PUT` + `If-Match`) a resource. The path `id`
 * addresses the item in both cases; `create` selects the method.
 */
export async function writeResource(
  kind: ResourceKind,
  id: string,
  input: ResourceInput,
  create: boolean,
  options: ResourceRequestOptions = {},
): Promise<ResourceWithEtag> {
  const response = await fetch(itemUrl(kind, id, options), {
    method: create ? 'POST' : 'PUT',
    headers: headersFor(options, true),
    body: JSON.stringify({ name: input.name, body: input.body }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isResourceRecord(body)) {
    throw new ResourceApiError('The server returned an unexpected resource body.');
  }
  const etag = response.headers.get('ETag');
  return { record: body, etag: etag ?? undefined };
}

/** Delete a resource (`DELETE /api/v1/{kind}/{id}`), sending `If-Match` when known. */
export async function deleteResource(
  kind: ResourceKind,
  id: string,
  options: ResourceRequestOptions = {},
): Promise<void> {
  const response = await fetch(itemUrl(kind, id, options), {
    method: 'DELETE',
    headers: headersFor(options, false),
  });
  // 404 is idempotent-success for a delete (the resource is already gone).
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

// --- body → view-model projections -----------------------------------------

function asSourceKind(value: string | undefined): SourceKind {
  // `ts` (MPEG-TS) is modelled under the generic `rtsp`-style URL inputs; map
  // any unrecognized/absent kind to `test`, the parameter-free built-in.
  return SOURCE_KINDS.find((k) => k === value) ?? 'test';
}

function asOutputKind(value: string | undefined): OutputKind {
  // The config wire kinds use snake_case (`rtsp_server`, `ll_hls`); fold them to
  // the display kinds the OutputView exposes.
  switch (value) {
    case 'rtsp_server':
      return 'rtsp';
    case 'll_hls':
      return 'll-hls';
    default:
      return OUTPUT_KINDS.find((k) => k === value) ?? 'rtsp';
  }
}

function asOverlayKind(value: string | undefined): OverlayKind {
  return OVERLAY_KINDS.find((k) => k === value) ?? 'label';
}

/** Project a source record's opaque body into the {@link SourceView}. */
export function toSourceView(record: ResourceRecord): SourceView {
  return {
    id: record.id,
    name: record.name,
    kind: asSourceKind(stringField(record.body, 'kind')),
    url: stringField(record.body, 'url'),
  };
}

/** Project an output record's opaque body into the {@link OutputView}. */
export function toOutputView(record: ResourceRecord): OutputView {
  return {
    id: record.id,
    name: record.name,
    kind: asOutputKind(stringField(record.body, 'kind')),
    // Absent `enabled` defaults to enabled, matching the engine's behaviour.
    enabled: boolField(record.body, 'enabled') ?? true,
  };
}

/** Project an overlay record's opaque body into the {@link OverlayView}. */
export function toOverlayView(record: ResourceRecord): OverlayView {
  return {
    id: record.id,
    name: record.name,
    kind: asOverlayKind(stringField(record.body, 'kind')),
    z: numberField(record.body, 'z') ?? 0,
  };
}
