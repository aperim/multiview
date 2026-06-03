// Typed control-plane API client.
//
// The `paths` type is generated verbatim from the control-plane OpenAPI spec
// (`docs/api/openapi.json`, produced by `cargo xtask gen-openapi`) via
// openapi-typescript — see `npm run generate:api`. openapi-fetch is a thin,
// fully-typed wrapper around `fetch` whose request/response shapes are derived
// from that spec, so the SPA and the Rust API are checked to agree at compile
// time. Do NOT hand-write request/response types here.
import createClient from 'openapi-fetch';

import type { paths } from './schema';

/** Options for constructing a {@link MosaicApiClient}. */
export interface ApiClientOptions {
  /**
   * Base URL the API is served from. Defaults to the same origin (`''`), which
   * the Vite dev server proxies to the control plane and which is correct when
   * the SPA is embedded in the `mosaic` binary.
   */
  readonly baseUrl?: string;
  /**
   * Optional bearer token. When present it is sent as
   * `Authorization: Bearer <token>` on every request (the control plane
   * authenticates reads of the layout/realtime surface).
   */
  readonly token?: string;
}

/** A typed openapi-fetch client bound to the control-plane `paths`. */
export type MosaicApiClient = ReturnType<typeof createClient<paths>>;

/**
 * Build a typed API client. The returned client's `GET`/`POST`/… methods only
 * accept paths and shapes that exist in the generated schema.
 */
export function createApiClient(options: ApiClientOptions = {}): MosaicApiClient {
  const headers: Record<string, string> = {};
  if (options.token !== undefined && options.token !== '') {
    headers.Authorization = `Bearer ${options.token}`;
  }
  return createClient<paths>({
    baseUrl: options.baseUrl ?? '',
    headers,
  });
}
