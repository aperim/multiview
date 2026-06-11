// useEngineEvents — the conflated `devices` topic (ADR-M008/ADR-RT007).
//
// `device.status` is a latest-wins lane scoped by the envelope `id`: each frame
// REPLACES that device's cached snapshot (the connect-time snapshot frames and
// later deltas are the same wire shape). `device.removed` drops the entry.
// Lossless lifecycle events (adopted/mode/error/removed) feed a bounded,
// newest-first session ring for the Events tab; `device.discovered` rows are
// correlated by the envelope `corr` (the scan's operation id). `$resync`
// rebuilds: every devices cache is discarded, never merged.
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import {
  DEVICE_EVENTS_QUERY_KEY,
  DEVICE_STATUS_QUERY_KEY,
  DISCOVERED_LIVE_QUERY_KEY,
  ENGINE_CLOCK_QUERY_KEY,
  useEngineEvents,
} from './useEngineEvents';
import type {
  DeviceEventEntry,
  EngineClockRef,
} from './useEngineEvents';
import type { DeviceDiscovered, DeviceStatus } from './generated-types';

class FakeWebSocket {
  static instances: FakeWebSocket[] = [];
  onopen: (() => void) | null = null;
  onmessage: ((event: MessageEvent<unknown>) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  readonly url: string;

  constructor(url: string) {
    this.url = url;
    FakeWebSocket.instances.push(this);
  }

  send(): void {
    // The hook never sends in these tests.
  }

  close(): void {
    // Closed by the hook on unmount; nothing to emit.
  }

  emit(text: string): void {
    this.onmessage?.(new MessageEvent<unknown>('message', { data: text }));
  }
}

function frame(
  seq: number,
  t: string,
  data: unknown,
  extra: Record<string, unknown> = {},
): string {
  return JSON.stringify({ v: 1, topic: 'devices', seq, ts: seq * 10, t, data, ...extra });
}

describe('useEngineEvents devices topic', () => {
  let client: QueryClient;

  beforeEach(() => {
    FakeWebSocket.instances = [];
    vi.stubGlobal('WebSocket', FakeWebSocket);
    client = new QueryClient();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    client.clear();
  });

  function wrapper({ children }: { children: ReactNode }): ReactNode {
    return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
  }

  function start(): { socket: FakeWebSocket; unmount: () => void } {
    const { unmount } = renderHook(() => useEngineEvents(), { wrapper });
    const socket = FakeWebSocket.instances[0];
    if (socket === undefined) {
      throw new Error('no socket');
    }
    act(() => {
      socket.onopen?.();
    });
    return { socket, unmount };
  }

  it('device.status upserts the per-device snapshot, latest wins', () => {
    const { socket, unmount } = start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.status',
          {
            device_id: 'dev-a',
            state: 'ONLINE',
            mode: 'decoder',
            temperature_c: 41.5,
            last_seen_ts: 90,
          },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(
          2,
          'device.status',
          { device_id: 'dev-a', state: 'DEGRADED', mode: 'decoder' },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(3, 'device.status', { device_id: 'dev-b', state: 'ADOPTING' }, { id: 'dev-b' }),
      );
    });
    const map = client.getQueryData<Record<string, DeviceStatus>>(DEVICE_STATUS_QUERY_KEY);
    expect(map?.['dev-a']?.state).toBe('DEGRADED');
    expect(map?.['dev-b']?.state).toBe('ADOPTING');
    unmount();
  });

  it('device.removed drops the snapshot and records a lifecycle event', () => {
    const { socket, unmount } = start();
    act(() => {
      socket.emit(
        frame(1, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, { id: 'dev-a' }),
      );
      socket.emit(frame(2, 'device.removed', { device_id: 'dev-a' }, { id: 'dev-a' }));
    });
    const map = client.getQueryData<Record<string, DeviceStatus>>(DEVICE_STATUS_QUERY_KEY);
    expect(map?.['dev-a']).toBeUndefined();
    const ring = client.getQueryData<readonly DeviceEventEntry[]>(DEVICE_EVENTS_QUERY_KEY);
    expect(ring?.[0]?.event).toEqual({ kind: 'removed', deviceId: 'dev-a' });
    unmount();
  });

  it('lifecycle events stack newest-first in the session ring', () => {
    const { socket, unmount } = start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.adopted',
          { device_id: 'dev-a', driver: 'zowietek', name: 'Foyer' },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(
          2,
          'device.mode',
          {
            device_id: 'dev-a',
            mode: 'encoder',
            phase: 'started',
            impact: 'dev',
            detail: 'restarting',
          },
          { id: 'dev-a' },
        ),
      );
      socket.emit(
        frame(
          3,
          'device.error',
          { device_id: 'dev-a', message: 'decode stalled', code: '00004' },
          { id: 'dev-a' },
        ),
      );
    });
    const ring = client.getQueryData<readonly DeviceEventEntry[]>(DEVICE_EVENTS_QUERY_KEY);
    expect(ring).toHaveLength(3);
    expect(ring?.[0]?.event.kind).toBe('error');
    expect(ring?.[1]?.event).toEqual({
      kind: 'mode',
      deviceId: 'dev-a',
      mode: 'encoder',
      phase: 'started',
      impact: 'dev',
      detail: 'restarting',
    });
    expect(ring?.[2]?.event.kind).toBe('adopted');
    unmount();
  });

  it('device.discovered rows correlate to their scan via the envelope corr', () => {
    const { socket, unmount } = start();
    act(() => {
      socket.emit(
        frame(
          1,
          'device.discovered',
          { address: '[fd00::42]:80', driver: 'zowietek', family: 'ipv6', name: 'box' },
          { corr: 'op-scan' },
        ),
      );
    });
    const byCorr = client.getQueryData<Record<string, readonly DeviceDiscovered[]>>(
      DISCOVERED_LIVE_QUERY_KEY,
    );
    expect(byCorr?.['op-scan']).toHaveLength(1);
    expect(byCorr?.['op-scan']?.[0]?.address).toBe('[fd00::42]:80');
    unmount();
  });

  it('tracks an engine-monotonic clock reference from envelope timestamps', () => {
    const { socket, unmount } = start();
    act(() => {
      socket.emit(
        frame(5, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, { id: 'dev-a' }),
      );
    });
    const clock = client.getQueryData<EngineClockRef>(ENGINE_CLOCK_QUERY_KEY);
    expect(clock?.engineTs).toBe(50);
    expect(typeof clock?.wallMs).toBe('number');
    unmount();
  });

  it('$resync discards every devices cache (rebuild, never merge)', () => {
    const { socket, unmount } = start();
    act(() => {
      socket.emit(
        frame(1, 'device.status', { device_id: 'dev-a', state: 'ONLINE' }, { id: 'dev-a' }),
      );
      socket.emit(
        frame(
          2,
          'device.discovered',
          { address: '[fd00::42]:80', driver: 'zowietek', family: 'ipv6' },
          { corr: 'op-scan' },
        ),
      );
      socket.emit(
        JSON.stringify({
          v: 1,
          topic: '$control',
          seq: 0,
          ts: 99,
          t: '$resync',
          data: { topics: ['devices'] },
        }),
      );
    });
    expect(
      client.getQueryData<Record<string, DeviceStatus>>(DEVICE_STATUS_QUERY_KEY),
    ).toBeUndefined();
    expect(
      client.getQueryData<Record<string, readonly DeviceDiscovered[]>>(
        DISCOVERED_LIVE_QUERY_KEY,
      ),
    ).toBeUndefined();
    unmount();
  });
});
