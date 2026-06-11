// Unit tests for the pure cast-form logic (DEV-D3): the client-side mirror of
// the control plane's `split_authority` (cast/media.rs — bracketed IPv6 first,
// port defaults to 8009), the start-form validation codes, the exact
// StartCastSessionRequest body mapping, and the target-choice fold (adopted
// cast devices + the untrusted discovery inventory, device entries winning on
// an address collision).
import { describe, expect, it } from 'vitest';

import {
  castStartFormToRequest,
  castTargetChoices,
  DEFAULT_CAST_PORT,
  emptyCastStartForm,
  parseCastAuthority,
  validateCastStartForm,
} from './forms';
import type { DeviceView } from '../devices/types';
import type { DiscoveredServiceView } from '../devices/api';

describe('parseCastAuthority', () => {
  it('defaults the CASTV2 port for a bare host', () => {
    expect(parseCastAuthority('tv.example')).toEqual({
      host: 'tv.example',
      port: DEFAULT_CAST_PORT,
    });
  });

  it('honours an explicit host:port (Cast groups advertise non-default ports)', () => {
    expect(parseCastAuthority('tv.example:9000')).toEqual({
      host: 'tv.example',
      port: 9000,
    });
  });

  it('accepts a bracketed IPv6 literal without a port', () => {
    expect(parseCastAuthority('[fd00::1]')).toEqual({
      host: 'fd00::1',
      port: DEFAULT_CAST_PORT,
    });
  });

  it('accepts a bracketed IPv6 literal with a port', () => {
    expect(parseCastAuthority('[2001:db8::20]:8009')).toEqual({
      host: '2001:db8::20',
      port: 8009,
    });
  });

  it('trims surrounding whitespace', () => {
    expect(parseCastAuthority('  [fd00::1]:9  ')).toEqual({
      host: 'fd00::1',
      port: 9,
    });
  });

  it('rejects empty and whitespace-only input', () => {
    expect(parseCastAuthority('')).toBeUndefined();
    expect(parseCastAuthority('   ')).toBeUndefined();
  });

  it('rejects an unbracketed IPv6 literal (brackets are the URL convention)', () => {
    expect(parseCastAuthority('fd00::1')).toBeUndefined();
    expect(parseCastAuthority('2001:db8::20')).toBeUndefined();
  });

  it('rejects junk after the closing bracket', () => {
    expect(parseCastAuthority('[fd00::1]x')).toBeUndefined();
  });

  it('rejects an empty bracketed host', () => {
    expect(parseCastAuthority('[]:8009')).toBeUndefined();
    expect(parseCastAuthority('[]')).toBeUndefined();
  });

  it('rejects an empty host before a port', () => {
    expect(parseCastAuthority(':8009')).toBeUndefined();
  });

  it('rejects a non-numeric or out-of-range port', () => {
    expect(parseCastAuthority('host:abc')).toBeUndefined();
    expect(parseCastAuthority('host:70000')).toBeUndefined();
    expect(parseCastAuthority('host:-1')).toBeUndefined();
    expect(parseCastAuthority('host:8.5')).toBeUndefined();
  });
});

describe('validateCastStartForm', () => {
  it('requires an address', () => {
    const form = { ...emptyCastStartForm(), output: 'hls-out' };
    expect(validateCastStartForm(form)).toEqual({ address: 'required' });
  });

  it('flags a malformed authority with the cast-authority code', () => {
    const form = {
      ...emptyCastStartForm(),
      address: 'fd00::1',
      output: 'hls-out',
    };
    expect(validateCastStartForm(form)).toEqual({ address: 'cast-authority' });
  });

  it('requires a rendition', () => {
    const form = { ...emptyCastStartForm(), address: '[fd00::1]:8009' };
    expect(validateCastStartForm(form)).toEqual({ output: 'required' });
  });

  it('passes a complete, well-formed form', () => {
    const form = {
      ...emptyCastStartForm(),
      address: '[2001:db8::20]:8009',
      name: 'Lounge TV',
      output: 'hls-out',
    };
    expect(validateCastStartForm(form)).toEqual({});
  });
});

describe('castStartFormToRequest', () => {
  it('builds the exact StartCastSessionRequest, omitting a blank name', () => {
    const form = {
      ...emptyCastStartForm(),
      address: ' [fd00::1]:8009 ',
      name: '  ',
      output: 'hls-out',
    };
    expect(castStartFormToRequest(form)).toEqual({
      address: '[fd00::1]:8009',
      output: 'hls-out',
    });
  });

  it('carries a trimmed name when one is given', () => {
    const form = {
      ...emptyCastStartForm(),
      address: '[fd00::1]',
      name: ' Lounge TV ',
      output: 'hls-out',
    };
    expect(castStartFormToRequest(form)).toEqual({
      address: '[fd00::1]',
      name: 'Lounge TV',
      output: 'hls-out',
    });
  });
});

function deviceView(overrides: Partial<DeviceView> & { id: string }): DeviceView {
  return {
    name: overrides.id,
    driver: 'cast',
    rawDriver: 'cast',
    address: undefined,
    desiredMode: undefined,
    editable: true,
    ...overrides,
  };
}

function discoveredView(
  overrides: Partial<DiscoveredServiceView> & { key: string },
): DiscoveredServiceView {
  return {
    name: overrides.key,
    host: 'tv.local.',
    driverKind: 'cast',
    serviceType: '_googlecast._tcp',
    port: 8009,
    primaryAddress: '[fd00::42]:8009',
    endpoints: [{ address: '[fd00::42]:8009', family: 'ipv6' }],
    lastSeenUnixNs: 1,
    ...overrides,
  };
}

describe('castTargetChoices', () => {
  it('offers adopted cast devices (with an address) before discovered targets', () => {
    const devices = [
      deviceView({ id: 'tv-1', name: 'Saved TV', address: '[fd00::21]:8009' }),
    ];
    const discovered = [
      discoveredView({ key: 'cast|den|_googlecast._tcp', name: 'Den TV' }),
    ];
    expect(castTargetChoices(devices, discovered)).toEqual([
      {
        key: 'device:tv-1',
        label: 'Saved TV — [fd00::21]:8009',
        address: '[fd00::21]:8009',
        name: 'Saved TV',
      },
      {
        key: 'discovered:cast|den|_googlecast._tcp',
        label: 'Den TV — [fd00::42]:8009',
        address: '[fd00::42]:8009',
        name: 'Den TV',
      },
    ]);
  });

  it('never offers non-cast drivers, address-less devices, or non-cast services', () => {
    const devices = [
      deviceView({ id: 'dec-1', driver: 'zowietek', rawDriver: 'zowietek', address: 'http://[fd00::9]' }),
      deviceView({ id: 'tv-2', name: 'No address yet' }),
    ];
    const discovered = [
      discoveredView({
        key: 'zowietek-control|box|_zowie._tcp',
        driverKind: 'zowietek-control',
      }),
    ];
    expect(castTargetChoices(devices, discovered)).toEqual([]);
  });

  it('deduplicates by address: the adopted device wins over the discovery hint', () => {
    const devices = [
      deviceView({ id: 'tv-1', name: 'Saved TV', address: '[fd00::42]:8009' }),
    ];
    const discovered = [
      discoveredView({ key: 'cast|samehost|_googlecast._tcp', name: 'Same TV' }),
    ];
    const choices = castTargetChoices(devices, discovered);
    expect(choices).toHaveLength(1);
    expect(choices.at(0)?.key).toBe('device:tv-1');
  });
});
