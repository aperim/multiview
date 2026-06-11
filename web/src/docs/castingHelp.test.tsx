// In-app help for casting (DEV-D3): /help/casting is registered (searchable +
// linkable, ADR-W016) and the copy keeps the honest doctrine — server-initiated
// CASTV2 to the Default Media Receiver (never browser tab casting), the
// seconds-class Tier-D latency truth (HLS segment buffering; LL-HLS does not
// auto-engage), the cross-VLAN mDNS reality with the manual-address escape
// hatch, save-as-device, and the real failure modes (preemption, sleep/IP
// change). Vendor-neutral wording: the protocol name only, no product
// marketing names.
import { describe, expect, it } from 'vitest';
import { render } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';

import { DOCS_REGISTRY } from './registry';
import { TestProviders } from '../test/render';

describe('casting help registration (ADR-W016)', () => {
  it('registers /help/casting', () => {
    expect(DOCS_REGISTRY.some((page) => page.path === '/help/casting')).toBe(true);
  });
});

describe('casting help copy doctrine', () => {
  async function renderCastingHelp(): Promise<string> {
    const { CastingHelpPage } = await import('../pages/docs/CastingHelpPage');
    render(
      <TestProviders>
        <MemoryRouter>
          <CastingHelpPage />
        </MemoryRouter>
      </TestProviders>,
    );
    return document.body.textContent ?? '';
  }

  it('states what casting is: server-initiated, not browser tab casting', async () => {
    const text = await renderCastingHelp();
    expect(text).toMatch(/Default Media Receiver/);
    expect(text).toMatch(/browser tab/i);
    expect(text).toMatch(/server/i);
  });

  it('tells the honest Tier-D latency truth', async () => {
    const text = await renderCastingHelp();
    expect(text).toMatch(/Tier D/);
    expect(text).toMatch(/6–30\s*s/);
    expect(text).toMatch(/segment/i);
    expect(text).toMatch(/LL-HLS/);
  });

  it('covers cross-VLAN mDNS invisibility and the manual-address escape hatch', async () => {
    const text = await renderCastingHelp();
    expect(text).toMatch(/VLAN/i);
    expect(text).toMatch(/mDNS/i);
    expect(text).toMatch(/8009/);
    // IPv6 leads the examples (ADR-0042).
    expect(text).toMatch(/\[2001:db8::20\]:8009/);
  });

  it('covers save-as-device and the failure modes honestly', async () => {
    const text = await renderCastingHelp();
    expect(text).toMatch(/save/i);
    expect(text).toMatch(/export/i);
    expect(text).toMatch(/preempt/i);
    expect(text).toMatch(/sleep/i);
  });

  it('stays vendor-neutral: the protocol name, never a product marketing name', async () => {
    const text = await renderCastingHelp();
    expect(text).toMatch(/Google Cast/);
    expect(text).not.toMatch(/chromecast/i);
  });
});
