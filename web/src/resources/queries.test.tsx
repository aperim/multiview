// Tests for the Sources/Outputs/Overlays resource hooks, with the HTTP layer
// mocked by MSW. These assert real behaviour: list reads project the opaque
// `{id,name,body}` records onto the typed view-models, create/update/delete call
// the right method + path (update echoing a fetched ETag as `If-Match`), and a
// server error surfaces as an `ApiError` with its status.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  ApiError,
  useDeleteResource,
  useOutputs,
  useOverlays,
  useSaveResource,
  useSources,
} from './queries';

const BASE = 'http://localhost';

interface Stored {
  id: string;
  name: string;
  body: Record<string, unknown>;
}

let sources: Stored[] = [];
let lastMethod: string | null = null;
let lastPath: string | null = null;
let lastIfMatch: string | null = null;
let lastBody: unknown = null;

const server = setupServer(
  // Sources collection + item.
  http.get(`${BASE}/api/v1/sources`, () => HttpResponse.json(sources)),
  http.get(`${BASE}/api/v1/sources/:id`, ({ params }) => {
    const found = sources.find((s) => s.id === String(params.id));
    if (found === undefined) {
      return HttpResponse.json(
        { type: '/problems/not-found', title: 'Not found', status: 404 },
        { status: 404 },
      );
    }
    return HttpResponse.json(found, { headers: { ETag: '"src-v1"' } });
  }),
  http.post(`${BASE}/api/v1/sources/:id`, async ({ request, params }) => {
    lastMethod = 'POST';
    lastPath = new URL(request.url).pathname;
    lastBody = await request.json();
    const id = String(params.id);
    const body = lastBody as { name: string; body: Record<string, unknown> };
    const created: Stored = { id, name: body.name, body: body.body };
    sources = [...sources, created];
    return HttpResponse.json(created, { status: 201, headers: { ETag: '"src-v2"' } });
  }),
  http.put(`${BASE}/api/v1/sources/:id`, async ({ request, params }) => {
    lastMethod = 'PUT';
    lastPath = new URL(request.url).pathname;
    lastIfMatch = request.headers.get('If-Match');
    lastBody = await request.json();
    const id = String(params.id);
    const body = lastBody as { name: string; body: Record<string, unknown> };
    const updated: Stored = { id, name: body.name, body: body.body };
    sources = sources.map((s) => (s.id === id ? updated : s));
    return HttpResponse.json(updated, { headers: { ETag: '"src-v3"' } });
  }),
  http.delete(`${BASE}/api/v1/sources/:id`, ({ request, params }) => {
    lastMethod = 'DELETE';
    lastPath = new URL(request.url).pathname;
    lastIfMatch = request.headers.get('If-Match');
    const id = String(params.id);
    sources = sources.filter((s) => s.id !== id);
    return new HttpResponse(null, { status: 204 });
  }),
  // Outputs + overlays list endpoints (projection coverage).
  http.get(`${BASE}/api/v1/outputs`, () =>
    HttpResponse.json([
      { id: 'prog', name: 'Program', body: { kind: 'rtsp_server' } },
      { id: 'hls', name: 'HLS', body: { kind: 'll_hls', enabled: false } },
    ]),
  ),
  http.get(`${BASE}/api/v1/overlays`, () =>
    HttpResponse.json([
      { id: 'clk', name: 'Clock', body: { kind: 'clock', z: 100 } },
    ]),
  ),
);

function wrapper(client: QueryClient): (props: { children: ReactNode }) => ReactNode {
  return function Wrapper({ children }: { children: ReactNode }): ReactNode {
    return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  };
}

function newClient(): QueryClient {
  return new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
}

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
  sources = [];
  lastMethod = null;
  lastPath = null;
  lastIfMatch = null;
  lastBody = null;
});
afterAll(() => {
  server.close();
});

describe('useSources', () => {
  it('projects the opaque body onto the source view-model', async () => {
    sources = [
      { id: 'cam-north', name: 'North', body: { kind: 'rtsp', url: 'rtsp://h/s' } },
      { id: 'bars', name: 'Bars', body: { kind: 'test' } },
    ];
    const qc = newClient();
    const { result } = renderHook(() => useSources({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data).toHaveLength(2);
    expect(result.current.data?.[0]).toEqual({
      id: 'cam-north',
      name: 'North',
      kind: 'rtsp',
      url: 'rtsp://h/s',
    });
    expect(result.current.data?.[1]?.kind).toBe('test');
    expect(result.current.data?.[1]?.url).toBeUndefined();
  });

  it('surfaces an RFC 9457 problem as an ApiError with its status', async () => {
    server.use(
      http.get(`${BASE}/api/v1/sources`, () =>
        HttpResponse.json(
          { type: '/problems/unauthorized', title: 'Unauthorized', status: 401 },
          { status: 401 },
        ),
      ),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSources({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isError).toBe(true);
    });
    expect(result.current.error).toBeInstanceOf(ApiError);
    expect(result.current.error?.status).toBe(401);
    expect(result.current.error?.message).toBe('Unauthorized');
  });
});

describe('useOutputs / useOverlays projection', () => {
  it('folds wire kinds and defaults enabled for outputs', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useOutputs({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]).toEqual({
      id: 'prog',
      name: 'Program',
      kind: 'rtsp',
      enabled: true,
    });
    expect(result.current.data?.[1]).toEqual({
      id: 'hls',
      name: 'HLS',
      kind: 'll-hls',
      enabled: false,
    });
  });

  it('reads the overlay stacking order from the body', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useOverlays({ baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await waitFor(() => {
      expect(result.current.isSuccess).toBe(true);
    });
    expect(result.current.data?.[0]).toEqual({
      id: 'clk',
      name: 'Clock',
      kind: 'clock',
      z: 100,
    });
  });
});

describe('useSaveResource', () => {
  it('creates with POST to the item path', async () => {
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    const saved = await result.current.mutateAsync({
      id: 'cam-1',
      create: true,
      input: { name: 'Cam 1', body: { kind: 'rtsp', url: 'rtsp://h/1' } },
    });
    expect(lastMethod).toBe('POST');
    expect(lastPath).toBe('/api/v1/sources/cam-1');
    expect(saved.id).toBe('cam-1');
    expect(lastBody).toEqual({ name: 'Cam 1', body: { kind: 'rtsp', url: 'rtsp://h/1' } });
  });

  it('updates with PUT and echoes a fetched ETag as If-Match', async () => {
    sources = [{ id: 'cam-1', name: 'Old', body: { kind: 'rtsp' } }];
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync({
      id: 'cam-1',
      create: false,
      input: { name: 'New', body: { kind: 'rtsp' } },
    });
    expect(lastMethod).toBe('PUT');
    expect(lastPath).toBe('/api/v1/sources/cam-1');
    // No cached ETag → the hook GETs the record first (which returns "src-v1").
    expect(lastIfMatch).toBe('"src-v1"');
  });

  it('surfaces a create conflict as an ApiError', async () => {
    server.use(
      http.post(`${BASE}/api/v1/sources/:id`, () =>
        HttpResponse.json(
          { type: '/problems/conflict', title: 'Conflict', status: 409 },
          { status: 409 },
        ),
      ),
    );
    const qc = newClient();
    const { result } = renderHook(() => useSaveResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await expect(
      result.current.mutateAsync({
        id: 'dupe',
        create: true,
        input: { name: 'Dupe', body: { kind: 'test' } },
      }),
    ).rejects.toMatchObject({ status: 409 });
  });
});

describe('useDeleteResource', () => {
  it('deletes with DELETE to the item path', async () => {
    sources = [{ id: 'cam-1', name: 'Drop', body: { kind: 'test' } }];
    const qc = newClient();
    const { result } = renderHook(() => useDeleteResource('sources', { baseUrl: BASE }), {
      wrapper: wrapper(qc),
    });
    await result.current.mutateAsync('cam-1');
    expect(lastMethod).toBe('DELETE');
    expect(lastPath).toBe('/api/v1/sources/cam-1');
    expect(sources).toHaveLength(0);
  });
});
