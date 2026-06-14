// Typed HTTP bindings for the display-node enrollment + screen-pairing surface
// (managed-devices.md §9, DEV-B6).
//
// A display node enrolls one of two ways: it presents a one-time bearer token
// the operator minted here (and copied onto the node), or it shows a six-char
// code on its screen that the operator types back in (screen pairing). This
// module owns the admin token lifecycle (mint / list / revoke), the pending
// pairing list, and the operator's `POST /devices/pair`. Request/response
// shapes reuse the generated `components['schemas']` types — never hand-written
// duplicates — through the shared helpers in ../api/operations. Wire rows are
// narrowed with typed field guards, never `as`-casts.
import type { components } from '../api/schema';
import { apiUrl, buildHeaders, readProblem } from '../api/operations';
import type { RequestOptions } from '../api/operations';

/** The one-time mint response — the bearer secret is shown here and never again. */
export type MintedToken = components['schemas']['MintedToken'];

/** The admin-facing lifecycle state of an enrollment token. */
export type TokenState = components['schemas']['TokenState'];

/** The admin metadata for one enrollment token (never the secret). */
export interface TokenSummaryView {
  readonly tokenId: string;
  readonly state: TokenState;
  readonly createdEpochS: number;
  readonly expiresEpochS: number;
  readonly usedBy: string | undefined;
}

/** One operator-facing pending screen-pairing row (metadata only, never the code). */
export interface PairingRequestView {
  readonly fingerprint: string;
  readonly model: string;
  readonly nodeName: string;
  readonly createdEpochS: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function numberOrZero(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

const TOKEN_STATES: readonly TokenState[] = ['pending', 'used', 'revoked', 'expired'];

function asTokenState(value: unknown): TokenState | undefined {
  return TOKEN_STATES.find((state) => state === value);
}

function isMintedToken(value: unknown): value is MintedToken {
  return (
    isRecord(value) &&
    typeof value.token === 'string' &&
    typeof value.token_id === 'string' &&
    typeof value.created_epoch_s === 'number' &&
    typeof value.expires_epoch_s === 'number'
  );
}

async function getJsonArray(
  path: string,
  options: RequestOptions,
): Promise<readonly Record<string, unknown>[]> {
  const response = await fetch(apiUrl(options, path), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  // The every-guard narrows the parsed array (the listResources pattern); a
  // response that is not an array of objects is treated as empty, never cast.
  if (!Array.isArray(body) || !body.every(isRecord)) {
    return [];
  }
  return body;
}

/**
 * `POST /api/v1/devices/enrollment-tokens` — mint a one-time enrollment token
 * (admin). The 201 body is the ONLY time the bearer secret (`token`) is shown;
 * the operator copies it onto the node now or never. An out-of-range `ttlSecs`
 * is rejected `422`.
 */
export async function mintEnrollmentToken(
  ttlSecs: number | undefined,
  options: RequestOptions = {},
): Promise<MintedToken> {
  const requestBody: components['schemas']['MintTokenRequest'] =
    ttlSecs === undefined ? {} : { ttl_secs: ttlSecs };
  const response = await fetch(
    apiUrl(options, '/api/v1/devices/enrollment-tokens'),
    {
      method: 'POST',
      headers: buildHeaders(options, true),
      body: JSON.stringify(requestBody),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isMintedToken(body)) {
    throw new Error('The server returned an unexpected mint-token body.');
  }
  return body;
}

/** `GET /api/v1/devices/enrollment-tokens` — list the tokens (admin; metadata only). */
export async function listEnrollmentTokens(
  options: RequestOptions = {},
): Promise<TokenSummaryView[]> {
  const rows = await getJsonArray('/api/v1/devices/enrollment-tokens', options);
  const tokens: TokenSummaryView[] = [];
  for (const raw of rows) {
    const state = asTokenState(raw.state);
    if (typeof raw.token_id === 'string' && state !== undefined) {
      tokens.push({
        tokenId: raw.token_id,
        state,
        createdEpochS: numberOrZero(raw.created_epoch_s),
        expiresEpochS: numberOrZero(raw.expires_epoch_s),
        usedBy: typeof raw.used_by === 'string' ? raw.used_by : undefined,
      });
    }
  }
  return tokens;
}

/** `DELETE /api/v1/devices/enrollment-tokens/{id}` — revoke a token (admin; 204). */
export async function revokeEnrollmentToken(
  tokenId: string,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(
    apiUrl(options, `/api/v1/devices/enrollment-tokens/${encodeURIComponent(tokenId)}`),
    { method: 'DELETE', headers: buildHeaders(options, false) },
  );
  // 404 is idempotent-success for a revoke (the token is already gone).
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

/** `GET /api/v1/devices/pairing-requests` — the pending screen pairings (admin). */
export async function listPairingRequests(
  options: RequestOptions = {},
): Promise<PairingRequestView[]> {
  const rows = await getJsonArray('/api/v1/devices/pairing-requests', options);
  const requests: PairingRequestView[] = [];
  for (const raw of rows) {
    if (typeof raw.fingerprint === 'string') {
      requests.push({
        fingerprint: raw.fingerprint,
        model: typeof raw.model === 'string' ? raw.model : '',
        nodeName: typeof raw.node_name === 'string' ? raw.node_name : '',
        createdEpochS: numberOrZero(raw.created_epoch_s),
      });
    }
  }
  return requests;
}

/** The `201` body of `POST /devices/pair`. */
export type PairResponse = components['schemas']['PairResponse'];

function isPairResponse(value: unknown): value is PairResponse {
  return isRecord(value) && typeof value.device_id === 'string';
}

/**
 * `POST /api/v1/devices/pair` — the operator completes a screen pairing by
 * typing back the six-character code the node shows. `404` means the code is
 * unknown/expired; `409` means the chosen `deviceId` already exists. The 201
 * body carries the bound `device_id`.
 */
export async function pairDevice(
  code: string,
  deviceId: string | undefined,
  displayName: string | undefined,
  options: RequestOptions = {},
): Promise<PairResponse> {
  const requestBody: components['schemas']['PairRequest'] = {
    code,
    ...(deviceId !== undefined && deviceId !== '' ? { device_id: deviceId } : {}),
    ...(displayName !== undefined && displayName !== '' ? { display_name: displayName } : {}),
  };
  const response = await fetch(apiUrl(options, '/api/v1/devices/pair'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify(requestBody),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isPairResponse(body)) {
    throw new Error('The server returned an unexpected pair-device body.');
  }
  return body;
}
