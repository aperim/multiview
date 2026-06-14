// SettingsPage × Display Nodes (DEV-B6): the enrollment-token surface mints a
// one-time bearer token (shown ONCE in a copyable field), lists existing
// tokens with their lifecycle state, and revokes a pending token. The node
// appliance section is an honest, non-fabricated download placeholder.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SettingsPage } from './SettingsPage';
import { I18nProvider as AppI18nProvider } from '../i18n/I18nProvider';
import { ThemeProvider } from '../theme/ThemeProvider';
import { renderWithProviders } from '../test/render';

const WATCH_STATUS = {
  active: false,
  path: null,
  applied_count: 0,
  last_applied: null,
  last_rejected: null,
  restart_pending: [],
};

let tokens: Record<string, unknown>[] = [];

// The boot-model card (ADR-W022) also mounts on this page; mock its read with a
// benign "not modeled" snapshot so its query never hits an unhandled request.
const BOOT_MODEL = {
  modeled: false,
  boot_path: null,
  start: null,
  resumed: false,
  resume_fallback: null,
  diverged_from_loaded: [],
  diverged_from_boot_file: null,
  boot_file_error: null,
  active_path: null,
  active_written_at_ms: null,
};

const server = setupServer(
  http.get('*/api/v1/config/watch-status', () => HttpResponse.json(WATCH_STATUS)),
  http.get('*/api/v1/config/boot-model', () => HttpResponse.json(BOOT_MODEL)),
  http.get('*/api/v1/devices/enrollment-tokens', () => HttpResponse.json(tokens)),
  http.get('*/api/v1/devices/pairing-requests', () => HttpResponse.json([])),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  tokens = [];
});
afterAll(() => {
  server.close();
});

function renderSettings(): void {
  renderWithProviders(
    <AppI18nProvider>
      <ThemeProvider>
        <MemoryRouter>
          <SettingsPage />
        </MemoryRouter>
      </ThemeProvider>
    </AppI18nProvider>,
  );
}

describe('SettingsPage display-nodes card', () => {
  it('shows the Display Nodes card with the honest node-appliance note', async () => {
    renderSettings();
    expect(
      await screen.findByRole('heading', { name: /display nodes/i }),
    ).toBeInTheDocument();
    // Honest appliance guidance, no fabricated working download.
    expect(screen.getByText(/flash the multiview node image/i)).toBeInTheDocument();
  });

  it('mints a token and shows the bearer secret exactly once in a copyable field', async () => {
    server.use(
      http.post('*/api/v1/devices/enrollment-tokens', () =>
        HttpResponse.json(
          {
            token_id: 'enr-abc',
            token: 'enr-abc.s3cr3t-once',
            created_epoch_s: 1_700_000_000,
            expires_epoch_s: 1_700_003_600,
          },
          { status: 201 },
        ),
      ),
    );
    renderSettings();
    await userEvent.click(
      await screen.findByRole('button', { name: /mint token/i }),
    );
    // The secret is shown in a read-only field the operator can copy now.
    expect(await screen.findByDisplayValue('enr-abc.s3cr3t-once')).toBeInTheDocument();
    // Dismissing clears the secret from the DOM (shown once).
    await userEvent.click(screen.getByRole('button', { name: /done/i }));
    expect(screen.queryByDisplayValue('enr-abc.s3cr3t-once')).not.toBeInTheDocument();
  });

  it('lists existing tokens and revokes a pending one', async () => {
    tokens = [
      {
        token_id: 'enr-pending',
        state: 'pending',
        created_epoch_s: 1_700_000_000,
        expires_epoch_s: 1_700_003_600,
      },
      {
        token_id: 'enr-used',
        state: 'used',
        created_epoch_s: 1_699_000_000,
        expires_epoch_s: 1_699_003_600,
        used_by: 'node-7',
      },
    ];
    let revoked = '';
    server.use(
      http.delete('*/api/v1/devices/enrollment-tokens/:id', ({ params }) => {
        revoked = String(params.id);
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderSettings();
    expect(await screen.findByText('enr-pending')).toBeInTheDocument();
    expect(screen.getByText('enr-used')).toBeInTheDocument();
    // The pending token can be revoked.
    const row = screen.getByText('enr-pending').closest('tr');
    expect(row).not.toBeNull();
    if (row === null) {
      throw new Error('expected a table row for the pending token');
    }
    await userEvent.click(within(row).getByRole('button', { name: /revoke/i }));
    expect(revoked).toBe('enr-pending');
  });
});
