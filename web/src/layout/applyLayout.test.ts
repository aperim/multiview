// Tests for the apply-layout command call: POST /api/v1/commands/apply-layout
// with body `{ layout }` (crates/multiview-control/src/routes/mod.rs
// `ApplyLayoutRequest`), returning `202 Accepted` + `{ operation_id, kind }`.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import { applyLayoutCommand } from './applyLayout';

let lastBody: unknown;
let lastAuth: string | null;

const server = setupServer(
  http.post('*/api/v1/commands/apply-layout', async ({ request }) => {
    lastBody = await request.json();
    lastAuth = request.headers.get('authorization');
    return HttpResponse.json(
      { operation_id: 'op-123', kind: 'apply_layout' },
      { status: 202 },
    );
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  lastBody = undefined;
  lastAuth = null;
});
afterAll(() => {
  server.close();
});

describe('applyLayoutCommand', () => {
  it('POSTs { layout } and returns the accepted operation id', async () => {
    const accepted = await applyLayoutCommand('wall-1', { token: 'tok-1' });
    expect(lastBody).toEqual({ layout: 'wall-1' });
    expect(lastAuth).toBe('Bearer tok-1');
    expect(accepted.operation_id).toBe('op-123');
    expect(accepted.kind).toBe('apply_layout');
  });

  it('surfaces an RFC 9457 problem on rejection', async () => {
    server.use(
      http.post('*/api/v1/commands/apply-layout', () =>
        HttpResponse.json(
          { title: 'Engine command bus at capacity', status: 503 },
          { status: 503 },
        ),
      ),
    );
    await expect(applyLayoutCommand('wall-1', { token: 'tok-1' })).rejects.toMatchObject({
      message: 'Engine command bus at capacity',
      status: 503,
    });
  });
});
