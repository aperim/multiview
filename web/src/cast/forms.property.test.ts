// Property tests (fast-check) for `parseCastAuthority` — the pure client-side
// mirror of the control plane's `split_authority` (cast/media.rs). The
// example-based suite lives in ./forms.test.ts; these properties pin the
// parser's algebra over the whole input space instead of hand-picked points:
//
//   1. totality      — never throws, on ANY string (ASCII or full unicode);
//   2. soundness     — anything accepted is a non-empty host + a u16 port;
//   3. completeness  — every bracketed IPv6 literal (± port) and every
//                      plain host (± port) parses to exactly its components;
//   4. port range    — a port above 65535 (or non-decimal) is ALWAYS rejected,
//                      bracketed or not;
//   5. round-trip    — re-serializing any parsed authority and re-parsing it
//                      yields the identical authority (parse ∘ print = id);
//   6. IPv6 brackets — a bare (unbracketed) IPv6 literal is ALWAYS rejected,
//                      never guessed at (ADR-0042: brackets required).
import * as fc from 'fast-check';
import { describe, expect, it } from 'vitest';

import { DEFAULT_CAST_PORT, parseCastAuthority } from './forms';

/** A valid port literal: the full inclusive u16 range. */
const portArb = fc.integer({ min: 0, max: 65_535 });

/**
 * A plain hostname-shaped host: DNS-ish characters only, so it is colon-free,
 * bracket-free, and stable under the parser's outer `trim()`. (The parser
 * itself accepts a wider host alphabet; the agreement property only needs a
 * generator whose serialized form is unambiguous.)
 */
const hostArb = fc.string({
  // ASCII-only alphabet, so Array.from decomposes it losslessly.
  unit: fc.constantFrom(...Array.from('abcdefghijklmnopqrstuvwxyz0123456789.-')),
  minLength: 1,
  maxLength: 64,
});

/** Arbitrary strings, biased to also cover full-unicode inputs. */
const anyStringArb = fc.oneof(fc.string(), fc.string({ unit: 'grapheme' }));

describe('parseCastAuthority (properties)', () => {
  it('is total and sound: never throws, and anything accepted is a non-empty host plus an in-range integer port', () => {
    fc.assert(
      fc.property(anyStringArb, (input) => {
        // Must not throw on ANY input string.
        const parsed = parseCastAuthority(input);
        if (parsed !== undefined) {
          expect(parsed.host).not.toBe('');
          expect(Number.isInteger(parsed.port)).toBe(true);
          expect(parsed.port).toBeGreaterThanOrEqual(0);
          expect(parsed.port).toBeLessThanOrEqual(65_535);
        }
      }),
    );
  });

  it('parses every bracketed IPv6 literal, with an explicit port and with the CASTV2 default', () => {
    fc.assert(
      fc.property(fc.ipV6(), portArb, (ip, port) => {
        expect(parseCastAuthority(`[${ip}]:${String(port)}`)).toEqual({
          host: ip,
          port,
        });
        expect(parseCastAuthority(`[${ip}]`)).toEqual({
          host: ip,
          port: DEFAULT_CAST_PORT,
        });
      }),
    );
  });

  it('parses every plain host to exactly its components, defaulting the port when omitted', () => {
    fc.assert(
      fc.property(hostArb, portArb, (host, port) => {
        expect(parseCastAuthority(`${host}:${String(port)}`)).toEqual({
          host,
          port,
        });
        expect(parseCastAuthority(host)).toEqual({
          host,
          port: DEFAULT_CAST_PORT,
        });
      }),
    );
  });

  it('always rejects an out-of-range port, bracketed or not', () => {
    fc.assert(
      fc.property(
        hostArb,
        fc.ipV6(),
        fc.integer({ min: 65_536, max: 2_147_483_647 }),
        (host, ip, port) => {
          expect(parseCastAuthority(`${host}:${String(port)}`)).toBeUndefined();
          expect(
            parseCastAuthority(`[${ip}]:${String(port)}`),
          ).toBeUndefined();
        },
      ),
    );
  });

  it('always rejects a non-decimal port literal', () => {
    fc.assert(
      fc.property(
        hostArb,
        // Anything that is not a pure decimal string once the parser's outer
        // trim has run (trailing whitespace of the port IS trimmed away as
        // part of the authority, so `"8 "` would legitimately parse).
        anyStringArb.filter((s) => !/^\d+$/.test(s.trim())),
        (host, badPort) => {
          expect(parseCastAuthority(`${host}:${badPort}`)).toBeUndefined();
        },
      ),
    );
  });

  it('round-trips: re-serializing any parsed authority parses back to the identical authority', () => {
    fc.assert(
      fc.property(anyStringArb, (input) => {
        const parsed = parseCastAuthority(input);
        fc.pre(parsed !== undefined);
        // Print the authority back to wire form. Brackets are required when
        // the host contains a colon (an IPv6 literal) and are the only
        // faithful form when the host is not trim-stable (such hosts can only
        // have come from a bracketed parse, so they never contain `]`).
        const needsBrackets =
          parsed.host.includes(':') || parsed.host.trim() !== parsed.host;
        const wire = needsBrackets
          ? `[${parsed.host}]:${String(parsed.port)}`
          : `${parsed.host}:${String(parsed.port)}`;
        expect(parseCastAuthority(wire)).toEqual(parsed);
      }),
    );
  });

  it('always rejects a bare (unbracketed) IPv6 literal instead of guessing', () => {
    fc.assert(
      fc.property(fc.ipV6(), (ip) => {
        expect(parseCastAuthority(ip)).toBeUndefined();
      }),
    );
  });
});
