// CastPanel — the ad-hoc cast surface (DEV-D3, ADR-M011): the ephemeral
// session list (state badge + the honest Tier-D latency badge, never colour
// alone), per-session stop and save-as-device, and the start sheet
// (pick a cast target or enter a manual `host[:port]` address — the
// cross-VLAN escape hatch, IPv6 bracketed first — pick a served HLS
// rendition, cast). Sessions ride the conflated `device.status` lane with the
// REST status snapshot as fallback; these tests run with no WebSocket, so the
// REST fallback and the session doc's own state feed the badges.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { CastPanel } from './CastPanel';
import { renderWithProviders } from '../test/render';

const SESSIONS = [
  {
    id: 'cast-session-1',
    name: 'Lounge TV',
    address: '[fd00::20]:8009',
    output: 'hls-out',
    media_url: 'http://[fd00::7]:8080/hls/hls-out/index.m3u8',
    state: 'ONLINE',
  },
];

const DEVICES = [
  {
    id: 'tv-1',
    name: 'Saved TV',
    body: { id: 'tv-1', driver: 'cast', address: '[fd00::21]:8009' },
  },
  {
    id: 'dec-1',
    name: 'Foyer decoder',
    body: { id: 'dec-1', driver: 'zowietek', address: 'http://[fd00::9]' },
  },
];

const DISCOVERED = [
  {
    key: 'cast|den|_googlecast._tcp',
    name: 'Den TV',
    host: 'den.local.',
    driver_kind: 'cast',
    service_type: '_googlecast._tcp',
    port: 8009,
    primary_address: '[fd00::42]:8009',
    endpoints: [{ address: '[fd00::42]:8009', family: 'ipv6' }],
    last_seen_unix_ns: 7,
  },
];

const OUTPUTS = [
  {
    id: 'hls-out',
    name: 'Program HLS',
    body: { id: 'hls-out', kind: 'hls', codec: 'h264', path: '/hls' },
  },
  {
    id: 'rtsp-out',
    name: 'Program RTSP',
    body: { id: 'rtsp-out', kind: 'rtsp_server', codec: 'h264', mount: '/mv' },
  },
];

const server = setupServer(
  http.get('*/api/v1/cast/sessions', () => HttpResponse.json(SESSIONS)),
  http.get('*/api/v1/devices', () => HttpResponse.json(DEVICES)),
  http.get('*/api/v1/discovery/devices', () => HttpResponse.json(DISCOVERED)),
  http.get('*/api/v1/outputs', () => HttpResponse.json(OUTPUTS)),
  http.get('*/api/v1/devices/cast-session-1/status', () =>
    HttpResponse.json({
      device_id: 'cast-session-1',
      state: 'ONLINE',
      mode: 'playing',
    }),
  ),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderPanel(): void {
  renderWithProviders(
    <MemoryRouter>
      <CastPanel />
    </MemoryRouter>,
  );
}

describe('CastPanel session list', () => {
  it('lists the live session with rendition, state text, and address', async () => {
    renderPanel();
    const panel = await screen.findByTestId('cast-panel');
    expect(await within(panel).findByText('Lounge TV')).toBeInTheDocument();
    expect(within(panel).getByText('[fd00::20]:8009')).toBeInTheDocument();
    expect(within(panel).getByText('hls-out')).toBeInTheDocument();
    // The lifecycle state is icon+text, never colour alone.
    expect(await within(panel).findByText('Online')).toBeInTheDocument();
  });

  it('carries the honest Tier-D latency badge on every session row', async () => {
    renderPanel();
    const panel = await screen.findByTestId('cast-panel');
    await within(panel).findByText('Lounge TV');
    // Honest seconds-class wording (managed-devices.md §8 Tier D): text, not
    // colour, carries the meaning.
    expect(within(panel).getByText(/Tier D/)).toBeInTheDocument();
    expect(within(panel).getByText(/6–30\s*s/)).toBeInTheDocument();
  });

  it('shows the empty state when no session is running', async () => {
    server.use(http.get('*/api/v1/cast/sessions', () => HttpResponse.json([])));
    renderPanel();
    expect(
      await screen.findByText('No cast sessions running.'),
    ).toBeInTheDocument();
  });

  it('shows the started-at age once the receiver accepted the LOAD', async () => {
    // started_unix_ns is Unix-epoch wall nanoseconds (the LOAD-accept stamp),
    // aged directly against wall time — unlike the engine-monotonic last-seen.
    const startedNs = (Date.now() - 90_000) * 1_000_000; // ~90 s ago
    server.use(
      http.get('*/api/v1/cast/sessions', () =>
        HttpResponse.json([{ ...SESSIONS[0], started_unix_ns: startedNs }]),
      ),
    );
    renderPanel();
    const panel = await screen.findByTestId('cast-panel');
    await within(panel).findByText('Lounge TV');
    // Honest relative readout: "started N min ago" (text, never colour alone).
    expect(await within(panel).findByText(/started.*min ago/i)).toBeInTheDocument();
  });

  it('shows "not started yet" when the LOAD has not been accepted (no fabricated time)', async () => {
    // The default SESSIONS fixture carries no started_unix_ns: the session is
    // establishing or its LOAD was refused — never invent a start instant.
    renderPanel();
    const panel = await screen.findByTestId('cast-panel');
    await within(panel).findByText('Lounge TV');
    expect(within(panel).getByText(/not started yet/i)).toBeInTheDocument();
    expect(within(panel).queryByText(/started.*ago/i)).not.toBeInTheDocument();
  });

  it('stops a session with DELETE /cast/sessions/{id}', async () => {
    let deleted = '';
    server.use(
      http.delete('*/api/v1/cast/sessions/:id', ({ params }) => {
        deleted = String(params.id);
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Stop cast: Lounge TV' }),
    );
    expect(deleted).toBe('cast-session-1');
  });
});

describe('CastPanel start sheet', () => {
  it('casts a manually-addressed device (the cross-VLAN escape hatch, IPv6 first)', async () => {
    let posted: unknown;
    server.use(
      http.post('*/api/v1/cast/sessions', async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(
          { ...SESSIONS[0], id: 'cast-session-2' },
          { status: 201 },
        );
      }),
    );
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Cast to a device…' }),
    );
    const dialog = await screen.findByRole('dialog');
    // The address field leads IPv6 (ADR-0042): bracketed literal, port 8009.
    // userEvent keyboard syntax: `[[` escapes a literal `[`; `]` is literal.
    await userEvent.type(
      within(dialog).getByLabelText('Device address'),
      '[[fd00::30]:8009',
    );
    await userEvent.type(
      within(dialog).getByLabelText('Session name (optional)'),
      'Bedroom',
    );
    await userEvent.click(within(dialog).getByRole('button', { name: 'Cast' }));
    // The first served HLS rendition is preselected — the body names it
    // explicitly rather than leaning on the server default.
    expect(posted).toEqual({
      address: '[fd00::30]:8009',
      name: 'Bedroom',
      output: 'hls-out',
    });
  });

  it('rejects an unbracketed IPv6 literal inline and posts nothing', async () => {
    let posts = 0;
    server.use(
      http.post('*/api/v1/cast/sessions', () => {
        posts += 1;
        return HttpResponse.json(SESSIONS[0], { status: 201 });
      }),
    );
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Cast to a device…' }),
    );
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(
      within(dialog).getByLabelText('Device address'),
      'fd00::30',
    );
    await userEvent.click(within(dialog).getByRole('button', { name: 'Cast' }));
    expect(
      await within(dialog).findByText(/wrap an IPv6 literal in brackets/i),
    ).toBeInTheDocument();
    expect(posts).toBe(0);
  });

  it('prefills the address from a picked target (adopted device or discovery hint)', async () => {
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Cast to a device…' }),
    );
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(
      within(dialog).getByRole('combobox', { name: 'Cast target' }),
    );
    await userEvent.click(
      await screen.findByRole('option', { name: /Saved TV/ }),
    );
    expect(
      within(dialog).getByDisplayValue('[fd00::21]:8009'),
    ).toBeInTheDocument();
  });

  it('refuses honestly when no HLS rendition is served', async () => {
    server.use(http.get('*/api/v1/outputs', () => HttpResponse.json([])));
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Cast to a device…' }),
    );
    const dialog = await screen.findByRole('dialog');
    expect(
      await within(dialog).findByText(/declare an HLS/i),
    ).toBeInTheDocument();
    expect(within(dialog).getByRole('button', { name: 'Cast' })).toBeDisabled();
  });
});

describe('CastPanel save-as-device', () => {
  it('promotes a session via POST /cast/sessions/{id}/save', async () => {
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
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Save as device: Lounge TV' }),
    );
    const dialog = await screen.findByRole('dialog');
    await userEvent.type(
      within(dialog).getByLabelText('Device identifier'),
      'tv-lounge',
    );
    // The display name is prefilled from the session's name.
    expect(within(dialog).getByDisplayValue('Lounge TV')).toBeInTheDocument();
    await userEvent.click(
      within(dialog).getByRole('button', { name: 'Save device' }),
    );
    expect(posted).toEqual({
      id: 'cast-session-1',
      payload: { device_id: 'tv-lounge', display_name: 'Lounge TV' },
    });
  });

  it('requires a device identifier before posting', async () => {
    let posts = 0;
    server.use(
      http.post('*/api/v1/cast/sessions/:id/save', () => {
        posts += 1;
        return HttpResponse.json(
          { id: 'tv-x', name: 'x', body: { id: 'tv-x', driver: 'cast' } },
          { status: 201 },
        );
      }),
    );
    renderPanel();
    await userEvent.click(
      await screen.findByRole('button', { name: 'Save as device: Lounge TV' }),
    );
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(
      within(dialog).getByRole('button', { name: 'Save device' }),
    );
    expect(
      await within(dialog).findByText('This field is required.'),
    ).toBeInTheDocument();
    expect(posts).toBe(0);
  });
});
