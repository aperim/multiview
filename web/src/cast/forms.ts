// Pure form-state logic for the ad-hoc cast sheet (DEV-D3, ADR-M011).
//
// Framework-free (no React, no Lingui) so the parsing and validation
// unit-test in isolation, exactly like ../devices/forms. The address rules
// mirror the control plane's `split_authority`
// (crates/multiview-control/src/devices/cast/media.rs) EXACTLY: a
// `host[:port]` authority, IPv6 bracketed first (`[2001:db8::20]:8009`,
// ADR-0042), the CASTV2 port 8009 as the default, and brackets required for
// IPv6 literals (the URL convention — a bare multi-colon host is rejected,
// never guessed at). The server re-validates with 422; mirroring here keeps
// the error at the field instead of a round trip.
import type { components } from '../api/schema';
import type { FieldErrors } from '../resources/forms';
import type { DiscoveredServiceView } from '../devices/api';
import type { DeviceView } from '../devices/types';

/** The default CASTV2 port (Cast groups advertise non-default ports). */
export const DEFAULT_CAST_PORT = 8009;

/** The highest valid TCP port (the authority port parses as a u16). */
const PORT_MAX = 65_535;

/** A parsed device authority: the dial host and CASTV2 port. */
export interface CastAuthority {
  /** The host (an IPv6 literal is stored WITHOUT its brackets). */
  readonly host: string;
  /** The CASTV2 port (8009 unless the authority named one). */
  readonly port: number;
}

/** Parse a decimal port string within `0..=65535`, or `undefined`. */
function parsePort(value: string): number | undefined {
  if (!/^\d+$/.test(value)) {
    return undefined;
  }
  const port = Number.parseInt(value, 10);
  return port <= PORT_MAX ? port : undefined;
}

/**
 * Parse a `host[:port]` device authority — the client-side mirror of the
 * control plane's `split_authority`. Bracketed IPv6 first; the port defaults
 * to {@link DEFAULT_CAST_PORT}; a bare IPv6 literal (more than one colon,
 * unbracketed) is rejected. Returns `undefined` for anything malformed.
 */
export function parseCastAuthority(address: string): CastAuthority | undefined {
  const trimmed = address.trim();
  if (trimmed === '') {
    return undefined;
  }
  if (trimmed.startsWith('[')) {
    // Bracketed IPv6: `[host]` or `[host]:port`.
    const close = trimmed.indexOf(']');
    if (close === -1) {
      return undefined;
    }
    const host = trimmed.slice(1, close);
    if (host === '') {
      return undefined;
    }
    const after = trimmed.slice(close + 1);
    if (after === '') {
      return { host, port: DEFAULT_CAST_PORT };
    }
    if (!after.startsWith(':')) {
      return undefined;
    }
    const port = parsePort(after.slice(1));
    return port === undefined ? undefined : { host, port };
  }
  const colon = trimmed.lastIndexOf(':');
  if (colon === -1) {
    return { host: trimmed, port: DEFAULT_CAST_PORT };
  }
  const host = trimmed.slice(0, colon);
  if (host === '' || host.includes(':')) {
    // More colons unbracketed would be a bare IPv6 literal — require
    // brackets for those (the URL convention).
    return undefined;
  }
  const port = parsePort(trimmed.slice(colon + 1));
  return port === undefined ? undefined : { host, port };
}

/** The editable state behind the ad-hoc cast sheet. */
export interface CastStartFormState {
  /** The device authority to dial (`host[:port]`, IPv6 bracketed). */
  readonly address: string;
  /** An operator-facing session name ('' = omit). */
  readonly name: string;
  /** The output id whose HLS rendition to cast. */
  readonly output: string;
}

/** The cast-sheet fields that can carry a validation error. */
export type CastStartField = 'address' | 'output';

/** A fresh, empty cast sheet. */
export function emptyCastStartForm(): CastStartFormState {
  return { address: '', name: '', output: '' };
}

/** Validate the cast sheet, returning per-field machine codes. */
export function validateCastStartForm(
  form: CastStartFormState,
): FieldErrors<CastStartField> {
  const errors: FieldErrors<CastStartField> = {};
  const address = form.address.trim();
  if (address === '') {
    errors.address = 'required';
  } else if (parseCastAuthority(address) === undefined) {
    errors.address = 'cast-authority';
  }
  if (form.output === '') {
    errors.output = 'required';
  }
  return errors;
}

/**
 * Build the exact `StartCastSessionRequest` body from a valid form. The
 * rendition is always named explicitly — the sheet preselects the first
 * served rendition rather than leaning on the server default, so what the
 * operator saw is what is cast.
 */
export function castStartFormToRequest(
  form: CastStartFormState,
): components['schemas']['StartCastSessionRequest'] {
  const name = form.name.trim();
  return {
    address: form.address.trim(),
    output: form.output,
    ...(name === '' ? {} : { name }),
  };
}

/** One pickable cast target in the sheet's target selector. */
export interface CastTargetChoice {
  /** Stable option key (`device:<id>` / `discovered:<key>`). */
  readonly key: string;
  /** The display label (`name — address`). */
  readonly label: string;
  /** The dial authority the choice prefills. */
  readonly address: string;
  /** The name the choice prefills. */
  readonly name: string;
}

/**
 * The pickable cast targets: adopted `cast`-driver devices (with an address)
 * first, then the untrusted discovery inventory's cast hints. Deduplicated by
 * address — the adopted device wins, since it is the operator-confirmed
 * record of the same box. Choices only ever PREFILL the sheet; the manual
 * address field stays the source of truth (the cross-VLAN escape hatch).
 */
export function castTargetChoices(
  devices: readonly DeviceView[],
  discovered: readonly DiscoveredServiceView[],
): CastTargetChoice[] {
  const choices: CastTargetChoice[] = [];
  const seen = new Set<string>();
  for (const device of devices) {
    if (device.driver !== 'cast' || device.address === undefined) {
      continue;
    }
    choices.push({
      key: `device:${device.id}`,
      label: `${device.name} — ${device.address}`,
      address: device.address,
      name: device.name,
    });
    seen.add(device.address);
  }
  for (const service of discovered) {
    if (service.driverKind !== 'cast' || seen.has(service.primaryAddress)) {
      continue;
    }
    choices.push({
      key: `discovered:${service.key}`,
      label: `${service.name} — ${service.primaryAddress}`,
      address: service.primaryAddress,
      name: service.name,
    });
    seen.add(service.primaryAddress);
  }
  return choices;
}
