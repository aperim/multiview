// DevicesPage × cast (DEV-D3): the Devices page hosts the cast surface — the
// ephemeral session list and the start sheet live in a panel above the fleet
// table (the DiscoveryPanel IA), with the in-app casting guide linked.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { DevicesPage } from './DevicesPage';
import { renderWithProviders } from '../test/render';

const server = setupServer(
  http.get('*/api/v1/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/sync-groups', () => HttpResponse.json([])),
  http.get('*/api/v1/discovery/devices', () => HttpResponse.json([])),
  http.get('*/api/v1/outputs', () => HttpResponse.json([])),
  http.get('*/api/v1/cast/sessions', () =>
    HttpResponse.json([
      {
        id: 'cast-session-1',
        name: 'Lounge TV',
        address: '[fd00::20]:8009',
        output: 'hls-out',
        media_url: 'http://[fd00::7]:8080/hls/hls-out/index.m3u8',
        state: 'ONLINE',
      },
    ]),
  ),
  http.get('*/api/v1/devices/cast-session-1/status', () =>
    HttpResponse.json({ device_id: 'cast-session-1', state: 'ONLINE' }),
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

describe('DevicesPage cast surface', () => {
  it('hosts the cast panel with the live session and the casting guide link', async () => {
    renderWithProviders(
      <MemoryRouter>
        <DevicesPage />
      </MemoryRouter>,
    );
    const panel = await screen.findByTestId('cast-panel');
    expect(await within(panel).findByText('Lounge TV')).toBeInTheDocument();
    const help = within(panel).getByRole('link', { name: 'About casting' });
    expect(help).toHaveAttribute('href', '/help/casting');
  });
});
