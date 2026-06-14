// DeviceDetailPage × display node (DEV-B6): the Display tab lists the node's
// reported scanout heads (connector, WxH, refresh in Hz, connected) and hosts
// the head-assignment editor that writes the device record's `display.assign`
// (Program / a named Output / a Wall head) via If-Match, merging into the
// existing body so the enrollment public key is never clobbered.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter, Route, Routes } from 'react-router-dom';

import { DeviceDetailPage } from './DeviceDetailPage';
import { renderWithProviders } from '../test/render';

const NODE = {
  id: 'node-a',
  name: 'Foyer wall node',
  body: {
    id: 'node-a',
    driver: 'displaynode',
    enrollment: { public_key: 'AAAAC3NzaC1lZDI1NTE5' },
    display: { assign: { program: true } },
  },
};

const ZOWIE = {
  id: 'dev-z',
  name: 'Vendor decoder',
  body: { id: 'dev-z', driver: 'zowietek', address: 'http://[fd00::9]' },
};

const server = setupServer(
  http.get('*/api/v1/devices/node-a', () =>
    HttpResponse.json(NODE, { headers: { ETag: '"7"' } }),
  ),
  http.get('*/api/v1/devices/dev-z', () =>
    HttpResponse.json(ZOWIE, { headers: { ETag: '"1"' } }),
  ),
  http.get('*/api/v1/devices/:id/status', () =>
    HttpResponse.json({ device_id: 'node-a', state: 'ONLINE' }),
  ),
  http.get('*/api/v1/sync-groups', () => HttpResponse.json([])),
  http.get('*/api/v1/devices/:id/source-candidates', () => HttpResponse.json([])),
  http.get('*/api/v1/devices/:id/output-targets', () => HttpResponse.json([])),
  http.get('*/api/v1/devices/node-a/display-heads', () =>
    HttpResponse.json([
      {
        id: 'head-0',
        connector: 'HDMI-A-1',
        width: 3840,
        height: 2160,
        refresh_millihertz: 60000,
        connected: true,
      },
      {
        id: 'head-1',
        connector: 'DP-1',
        width: 1920,
        height: 1080,
        refresh_millihertz: 50000,
        connected: false,
      },
    ]),
  ),
  http.get('*/api/v1/devices/dev-z/display-heads', () => HttpResponse.json([])),
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

function renderNode(id: string): void {
  renderWithProviders(
    <MemoryRouter initialEntries={[`/devices/${id}?tab=display`]}>
      <Routes>
        <Route path="/devices/:id" element={<DeviceDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe('DeviceDetailPage display tab', () => {
  it('exposes a Display tab for a display node', async () => {
    renderNode('node-a');
    expect(await screen.findByText('Foyer wall node')).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Display' })).toBeInTheDocument();
  });

  it('lists the reported heads with connector, resolution, Hz and connected state', async () => {
    renderNode('node-a');
    expect(await screen.findByText('HDMI-A-1')).toBeInTheDocument();
    expect(screen.getByText('3840×2160')).toBeInTheDocument();
    // 60_000 mHz renders as 60 Hz (never a float).
    expect(screen.getByText(/60 Hz/)).toBeInTheDocument();
    expect(screen.getByText('DP-1')).toBeInTheDocument();
    expect(screen.getByText(/50 Hz/)).toBeInTheDocument();
    // The connected head is badged; the disconnected one is not "connected".
    expect(screen.getByText(/^connected$/i)).toBeInTheDocument();
  });

  it('saves a wall-head assignment merging into the existing body (keeps the public key)', async () => {
    let putBody: unknown;
    server.use(
      http.put('*/api/v1/devices/node-a', async ({ request }) => {
        putBody = await request.json();
        return HttpResponse.json(NODE, { headers: { ETag: '"8"' } });
      }),
    );
    renderNode('node-a');
    // Choose the Wall head assignment, type the head id, save.
    const wall = await screen.findByLabelText(/wall head/i);
    await userEvent.click(wall);
    await userEvent.type(screen.getByLabelText(/wall head id/i), 'head-0');
    await userEvent.click(screen.getByRole('button', { name: /save assignment/i }));

    expect(putBody).toEqual({
      name: 'Foyer wall node',
      body: {
        id: 'node-a',
        driver: 'displaynode',
        enrollment: { public_key: 'AAAAC3NzaC1lZDI1NTE5' },
        display: { assign: { wall_head: 'head-0' } },
      },
    });
  });

  it('is honest for a non-display-node device', async () => {
    renderNode('dev-z');
    expect(await screen.findByText('Vendor decoder')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('tab', { name: 'Display' }));
    expect(screen.getByText(/not a display node/i)).toBeInTheDocument();
  });
});
