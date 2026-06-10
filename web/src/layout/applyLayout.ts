// The apply-layout engine command.
//
// `POST /api/v1/commands/apply-layout` with body `{ layout: <layout id> }`
// (crates/multiview-control/src/routes/mod.rs `ApplyLayoutRequest`). The
// control plane answers `202 Accepted` + `{ operation_id, kind }`; the actual
// outcome arrives later on the realtime stream correlated by that operation id
// (ADR-W008, invariant #10 — the request never blocks the engine). Unlike the
// stored-resource CRUD (which applies via config export + restart), apply-layout
// IS a live action.
//
// `submitOperation` in ../api/operations only posts body-less commands, so this
// reuses its header/url/problem helpers with an explicit JSON body.
import { apiUrl, buildHeaders, OperationApiError, readProblem } from '../api/operations';
import type { AcceptedBody, RequestOptions } from '../api/operations';

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isAcceptedBody(value: unknown): value is AcceptedBody {
  return (
    isRecord(value) &&
    typeof value.operation_id === 'string' &&
    typeof value.kind === 'string'
  );
}

/**
 * Apply the layout with `layoutId` to the running multiview. Resolves with the
 * accepted operation id; rejects with an `OperationApiError` carrying the RFC
 * 9457 problem title/status on refusal (401/403/503).
 */
export async function applyLayoutCommand(
  layoutId: string,
  options: RequestOptions = {},
): Promise<AcceptedBody> {
  const response = await fetch(apiUrl(options, '/api/v1/commands/apply-layout'), {
    method: 'POST',
    headers: buildHeaders(options, true),
    body: JSON.stringify({ layout: layoutId }),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isAcceptedBody(body)) {
    throw new OperationApiError('The server returned an unexpected command body.');
  }
  return body;
}
