// Unit tests for the cast-session API bindings (DEV-D3): doc → view
// projection (typed guards, never `as`-casts), the DeviceState fold for the
// session badge, and the typed HTTP calls (list/start/stop/save) against an
// MSW double of the control plane — including the RFC 9457 detail surfacing
// (the problem `title` is generic; `detail` carries the operator-facing why).
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';

import {
  asDeviceState,
  listCastSessions,
  operationErrorMessage,
  saveCastSession,
  startCastSession,
  stopCastSession,
  toCastSessionView,
} from './api';

const server = setupServer();

beforeAll(() => {
  server.listen({ onUnhandledRequest: 'error' });
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

const SESSION_DOC = {
  id: 'cast-session-1',
  name: 'Lounge TV',
  address: '[fd00::20]:8009',
  output: 'hls-out',
  media_url: 'http://[fd00::7]:8080/hls/hls-out/index.m3u8',
  state: 'ONLINE',
};

describe('toCastSessionView', () => {
  it('projects the wire doc onto the view', () => {
    expect(toCastSessionView(SESSION_DOC)).toEqual({
      id: 'cast-session-1',
      name: 'Lounge TV',
      address: '[fd00::20]:8009',
      output: 'hls-out',
      mediaUrl: 'http://[fd00::7]:8080/hls/hls-out/index.m3u8',
      state: 'ONLINE',
    });
  });

  it('folds an absent name to undefined (never an empty string)', () => {
    const { name, ...rest } = SESSION_DOC;
    void name;
    expect(toCastSessionView({ ...rest, name: null }).name).toBeUndefined();
    expect(toCastSessionView(rest).name).toBeUndefined();
  });
});

describe('asDeviceState', () => {
  it('accepts the DEV-A3 wire vocabulary', () => {
    expect(asDeviceState('ONLINE')).toBe('ONLINE');
    expect(asDeviceState('ADOPTING')).toBe('ADOPTING');
    expect(asDeviceState('DEGRADED')).toBe('DEGRADED');
  });

  it('folds an unknown token to undefined — a state is never invented', () => {
    expect(asDeviceState('PLAYING')).toBeUndefined();
    expect(asDeviceState('online')).toBeUndefined();
    expect(asDeviceState('')).toBeUndefined();
  });
});

describe('listCastSessions', () => {
  it('lists sessions, dropping malformed rows rather than fabricating them', async () => {
    server.use(
      http.get('*/api/v1/cast/sessions', () =>
        HttpResponse.json([SESSION_DOC, { id: 'broken' }]),
      ),
    );
    const sessions = await listCastSessions();
    expect(sessions).toHaveLength(1);
    expect(sessions.at(0)?.id).toBe('cast-session-1');
  });
});

describe('startCastSession', () => {
  it('POSTs the request body and returns the started session view', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/cast/sessions', async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(SESSION_DOC, { status: 201 });
      }),
    );
    const view = await startCastSession({
      address: '[fd00::20]:8009',
      name: 'Lounge TV',
      output: 'hls-out',
    });
    expect(posted).toEqual({
      address: '[fd00::20]:8009',
      name: 'Lounge TV',
      output: 'hls-out',
    });
    expect(view.id).toBe('cast-session-1');
  });

  it('surfaces the RFC 9457 detail on a conflict (the title is generic)', async () => {
    server.use(
      http.post('*/api/v1/cast/sessions', () =>
        HttpResponse.json(
          {
            type: '/problems/conflict',
            title: 'Conflict with current state',
            status: 409,
            detail: 'no castable HLS rendition: set control.cast_media_base',
          },
          { status: 409 },
        ),
      ),
    );
    const failure = await startCastSession({ address: '[fd00::20]' }).then(
      () => undefined,
      (error: unknown) => error,
    );
    expect(failure).toBeDefined();
    expect(operationErrorMessage(failure)).toBe(
      'no castable HLS rendition: set control.cast_media_base',
    );
  });
});

describe('stopCastSession', () => {
  it('DELETEs the session', async () => {
    let deleted = '';
    server.use(
      http.delete('*/api/v1/cast/sessions/:id', ({ params }) => {
        deleted = String(params.id);
        return new HttpResponse(null, { status: 204 });
      }),
    );
    await stopCastSession('cast-session-1');
    expect(deleted).toBe('cast-session-1');
  });

  it('treats 404 as success — the session is already gone', async () => {
    server.use(
      http.delete('*/api/v1/cast/sessions/:id', () =>
        HttpResponse.json(
          { type: '/problems/not-found', title: 'Resource not found', status: 404 },
          { status: 404 },
        ),
      ),
    );
    await expect(stopCastSession('cast-session-1')).resolves.toBeUndefined();
  });
});

describe('saveCastSession', () => {
  it('POSTs the promotion body and returns the created device record', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/cast/sessions/:id/save', async ({ request, params }) => {
        posted = { id: params.id, payload: await request.json() };
        return HttpResponse.json(
          {
            id: 'tv-lounge',
            name: 'Lounge TV',
            body: { id: 'tv-lounge', driver: 'cast', address: '[fd00::20]:8009' },
          },
          { status: 201, headers: { ETag: '"1"' } },
        );
      }),
    );
    const record = await saveCastSession('cast-session-1', {
      device_id: 'tv-lounge',
      display_name: 'Lounge TV',
    });
    expect(posted).toEqual({
      id: 'cast-session-1',
      payload: { device_id: 'tv-lounge', display_name: 'Lounge TV' },
    });
    expect(record.id).toBe('tv-lounge');
  });

  it('rejects with the conflict detail when the device id already exists', async () => {
    server.use(
      http.post('*/api/v1/cast/sessions/:id/save', () =>
        HttpResponse.json(
          {
            type: '/problems/conflict',
            title: 'Conflict with current state',
            status: 409,
            detail: 'device "tv-lounge" already exists',
          },
          { status: 409 },
        ),
      ),
    );
    const failure = await saveCastSession('cast-session-1', {
      device_id: 'tv-lounge',
    }).then(
      () => undefined,
      (error: unknown) => error,
    );
    expect(operationErrorMessage(failure)).toBe('device "tv-lounge" already exists');
  });
});
