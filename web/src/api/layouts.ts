// Layouts CRUD bindings over the typed control-plane client.
//
// SCHEMA STATUS (read me)
// -----------------------
// The generated OpenAPI schema (`schema.ts`) currently only declares
// `GET /api/v1/layouts`. The control plane *does* model the create/update bodies
// (`LayoutInput`) and the resource (`Layout`) in `components.schemas`, but the
// `POST`/`PUT`/`DELETE` path operations are not in the spec yet. So:
//
//   * the LIST read goes through the fully-typed `openapi-fetch` client; and
//   * create/update/delete go through a thin, EXPLICITLY-TYPED view-model that
//     reuses the generated `LayoutInput` request type and `Layout` response type
//     (NOT hand-written shapes, and NOT a fake `as`-cast of an untyped body).
//
// TODO(api-schema): once `cargo xtask gen-openapi` emits the write operations,
// delete `writeLayout`/`deleteLayout` here and call `client.POST/PUT/DELETE`
// directly so these become compile-checked against the spec like the list read.
//
// ETag/If-Match: the control plane version-stamps a layout with an `ETag` and
// requires `If-Match` on update/delete (conventions §6, RFC 9457 on conflict).
// We read the ETag from the list response cache (per resource) and echo it back.
import type { components } from './schema';

/** A persisted layout resource, exactly as the control plane returns it. */
export type Layout = components['schemas']['Layout'];

/** The create/update payload accepted by the control plane. */
export type LayoutInput = components['schemas']['LayoutInput'];

/** An RFC 9457 problem document, as modelled in the spec. */
export type Problem = components['schemas']['Problem'];

/** A failed control-plane call, normalized to a message + status (+ optional ETag). */
export class LayoutApiError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'LayoutApiError';
    this.status = status;
  }
}

/** Base path for the layouts collection (REST base `/api/v1`, conventions §6). */
const LAYOUTS_PATH = '/api/v1/layouts';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isProblem(value: unknown): value is Problem {
  return (
    isRecord(value) &&
    typeof value.status === 'number' &&
    typeof value.title === 'string'
  );
}

function isLayout(value: unknown): value is Layout {
  return (
    isRecord(value) &&
    typeof value.id === 'string' &&
    typeof value.name === 'string'
  );
}

async function readProblem(response: Response): Promise<LayoutApiError> {
  try {
    const body: unknown = await response.json();
    if (isProblem(body)) {
      return new LayoutApiError(body.title, body.status);
    }
  } catch {
    // Fall through to a status-only error when the body is absent/unparseable.
  }
  return new LayoutApiError(`Request failed (${String(response.status)})`, response.status);
}

/** Options shared by the write helpers. */
export interface LayoutWriteOptions {
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

function headersFor(
  options: LayoutWriteOptions,
  withJsonBody: boolean,
): Headers {
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

function urlFor(options: LayoutWriteOptions, id?: string): string {
  const base = options.baseUrl ?? '';
  const collection = `${base}${LAYOUTS_PATH}`;
  return id === undefined ? collection : `${collection}/${encodeURIComponent(id)}`;
}

/** The result of a successful write: the stored layout and its new ETag. */
export interface LayoutWriteResult {
  /** The stored resource as the control plane returned it. */
  readonly layout: Layout;
  /** The new ETag, when the response carried one. */
  readonly etag: string | undefined;
}

/**
 * Create (`id === undefined`) or update (`id` given) a layout. Reuses the
 * generated `LayoutInput`/`Layout` types so the payload + response stay checked
 * against the schema even though the path op is not in `paths` yet.
 */
export async function writeLayout(
  input: LayoutInput,
  options: LayoutWriteOptions = {},
  id?: string,
): Promise<LayoutWriteResult> {
  const response = await fetch(urlFor(options, id), {
    method: id === undefined ? 'POST' : 'PUT',
    headers: headersFor(options, true),
    body: JSON.stringify(input),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isLayout(body)) {
    throw new LayoutApiError('The server returned an unexpected layout body.');
  }
  const etag = response.headers.get('ETag');
  return { layout: body, etag: etag ?? undefined };
}

/** Delete a layout by id, sending `If-Match` when an ETag is known. */
export async function deleteLayout(
  id: string,
  options: LayoutWriteOptions = {},
): Promise<void> {
  const response = await fetch(urlFor(options, id), {
    method: 'DELETE',
    headers: headersFor(options, false),
  });
  // 404 is idempotent-success for a delete (the resource is already gone).
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}
