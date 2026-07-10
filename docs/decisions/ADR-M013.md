# ADR-M013: Outbound-dial SSRF guard for managed devices + Cast sessions

- **Status:** Accepted
- **Area:** Management
- **Date:** 2026-07-10
- **Source:** security audit (SSRF review of `multiview-control`, findings SEC-02 CRITICAL + SEC-04 MEDIUM, CWE-918/CWE-20)

## Context

The control plane dials operator-supplied addresses on two paths:

- The managed-device poller (`zowietek` driver) `POST`s to `<device.address>/…`
  over reqwest, sending the device's resolved `(username, password)` in cleartext
  ([ADR-M009](ADR-M009.md)).
- The ad-hoc Cast session actor opens TCP+TLS to `POST /api/v1/cast/sessions`'s
  `address` ([ADR-M011](ADR-M011.md)).

Before this ADR the address was only shape-checked: `Device::validate`
(`multiview-config`) rejected an empty string; `start_cast_session` only ran
`split_authority`. A **Write-role** principal (or a stolen Write API key) could
therefore point either dial at:

- **SEC-02 (CRITICAL):** `http://169.254.169.254/…` — the cloud-metadata (IMDS)
  endpoint — to steal instance credentials, and the reqwest client followed up to
  10 redirects, so even a validated host could 30x-bounce there. The device's
  named credentials are exfiltrated in cleartext to the attacker-chosen host.
- **SEC-04 (MEDIUM):** `[fd00::5]:6379` and similar — an internal TCP+TLS
  port-scan primitive (Online/Unreachable is a non-blind oracle).

The dial target is **orthogonal to the object-authz model**, so a scoped
operator escapes its object blast radius.

The binding constraint that shapes the fix: **managed devices legitimately live
on the LAN.** The canonical device example is a ULA address
(`http://[fd00:db8::42]`), and ~30 existing tests + the managed-devices brief use
private/ULA device addresses. A naive "block all private" would break the entire
device feature and the green suite. Invariant #10 (isolation) also holds: all of
this is control-plane, never touching the engine.

## Decision

A shared **two-layer** dial guard, `multiview_config::device::net_guard`,
consulted by config load and by every outbound-dial site.

**Layer 1 — config load (`Device::validate`, offline, deployment-independent).**
Reject only the **never-legitimate** literal dial targets — loopback, link-local
(incl. IMDS `169.254.169.254`), unspecified, multicast, broadcast, and their
IPv4-mapped-IPv6 forms — and, for a device-management address that carries a
URL scheme, enforce an `http`/`https` **scheme allowlist** (blocking
`file:`/`gopher:` gadgets; a scheme-less `host[:port]` authority, as discovery
emits, is accepted). Private/ULA/carrier LAN literals **pass** here — whether
they may be dialled is a runtime policy decision, not a config-syntax one.

**Layer 2 — dial time (`screen_ip`/`screen_resolved` on the RESOLVED IP).**
Screen the resolved address against a **strict default-deny of every internal
range** (the never-legitimate set **plus** RFC-1918 private, ULA `fc00::/7`, and
RFC-6598 carrier-grade-NAT), minus an operator CIDR **allowlist**
(`DialPolicy`). The caller then dials only the vetted, **pinned** IP. Screening
the resolved answer (not the hostname) is the only thing that defeats
**DNS-rebind**: a public name that answers with a private/loopback address is
refused. Applied at:

- `start_cast_session` — resolve + screen **before** the actor spawns (422, no
  record).
- `TlsCastConnector::connect` (`cast` feature) — resolve + screen + pin on
  **every** (re)connect.
- The `zowietek` reqwest client (`zowietek` feature) — a custom
  `reqwest::dns::Resolve` screens every resolved IP (each request/redirect), plus
  `redirect(Policy::none())`.

**Operator allowlist source.** `AppState.dial_policy` defaults to deny-internal;
the binary widens it from the `MULTIVIEW_DEVICE_DIAL_ALLOW` environment variable
(comma/whitespace-separated CIDRs, e.g. `192.168.0.0/16, fd00:db8::/32`),
fail-closed on a bad entry. The never-legitimate ranges are **not** allowlistable.
`ipnet` (MIT/Apache-2.0, already in the lock) parses the CIDRs.

## Rationale

- **The CRITICAL vector is closed with zero feature breakage.** IMDS is
  link-local, so blocking the never-legitimate ranges (at both layers) stops
  cloud-credential theft without touching the LAN ranges real devices sit in.
- **Config validation stays deployment-independent.** A config valid on one host
  is valid on another; the LAN dial policy is a runtime concern. This keeps the
  ULA example + the existing device/cast suites green (no test-weakening,
  rule 19).
- **Resolved-IP screening is the only correct DNS-rebind defence** — a literal or
  hostname check at config load cannot see the rebind. Pinning the vetted IP for
  the dial closes the resolve→connect TOCTOU.
- **`redirect(Policy::none())`** prevents a validated host from bouncing the
  poller — and its cleartext credentials — to a blocked one.
- **Default-deny + operator allowlist** is the secure-by-default posture the
  audit recommended ("reject internal ranges unless an operator allowlist opts
  in"): out of the box the SSRF surface is closed; an operator re-enables exactly
  their device subnet.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| Block all private/ULA at `Device::validate` | Breaks the managed-device feature (devices live on the LAN) and ~30 green tests + the ULA brief example; config load would fail on upgrade. |
| Guard only at config load (literal/hostname screen) | Cannot defeat DNS-rebind (the name resolves differently at dial), and exotic literal encodings (`0x7f.0.0.1`, `2130706433`) differ from what the dialer resolves. The resolved-IP screen is differential-proof. |
| Hard cast port allowlist (8009 only) | Cast **groups** advertise dynamic ephemeral ports; a strict allowlist breaks multizone. The resolved-IP screen already blocks SEC-04's `[fd00::5]:6379` (the host is ULA), so the port allowlist adds little and costs a feature. |
| Allowlist in `control.device_dial_allow` (config-as-code) | Preferred permanent home, but it edits `ControlConfig` in `lib.rs`, which the in-flight W026 lane owns; deferred to avoid a hot-file collision. The env-var source is a complete, working escape hatch (consistent with the existing `env:` credential resolver) and migrates to config later. |

## Consequences

- **Secure by default:** with `MULTIVIEW_DEVICE_DIAL_ALLOW` unset, only
  globally-routable hosts are dialled. An operator whose managed devices sit on a
  private/ULA LAN (the common case, and only in a `devices-net`/`zowietek`/`cast`
  build) **must** set the env var to their device subnet, or those devices stay
  `UNREACHABLE` with a warning. Runbook:
  [`docs/runbooks/device-dial-allowlist.md`](../runbooks/device-dial-allowlist.md).
- **Follow-up:** promote the allowlist to a `control.device_dial_allow` config
  field once the W026 `ControlConfig` edits land (config-as-code + UI surface).
- **Residual (documented):** dialling a **globally-routable** host on an arbitrary
  port is still allowed (any client can already reach public hosts); this is not
  an SSRF escalation. A rebind to an **allowlisted** internal range is permitted
  by definition (the operator opted in).
- Adds a direct `ipnet` dependency to `multiview-config` (deny-clean). No new
  network I/O in `multiview-config` (resolution happens at the async dial sites);
  config validation stays pure/offline.
- Invariant #10 holds: `dial_policy` is read-only control-plane state; the guard
  runs on the control/IO plane and never touches the engine.
