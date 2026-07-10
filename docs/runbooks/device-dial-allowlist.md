# Runbook — managed-device / Cast outbound-dial allowlist

**What & why.** The control plane screens every outbound dial (managed-device
poller + ad-hoc Cast session actor) against an SSRF guard ([ADR-M013](../decisions/ADR-M013.md),
SEC-02/SEC-04, CWE-918). By default it dials **only globally-routable hosts**:
loopback, link-local (incl. the cloud-metadata IP `169.254.169.254`), unspecified,
multicast/broadcast, **and** private (RFC 1918), ULA (`fc00::/7`), and
carrier-grade-NAT (`100.64.0.0/10`) targets are refused on the **resolved** IP.

Because real managed devices (ZowieBox encoders, Cast TVs) live on the LAN
— private/ULA addresses — a `devices-net`/`zowietek`/`cast` build must be told
which internal subnets are legitimate, or those devices stay `UNREACHABLE`.

## The knob

Environment variable, read once at control-plane start:

```
MULTIVIEW_DEVICE_DIAL_ALLOW="<cidr>[,<cidr>...]"
```

- Comma- or whitespace-separated CIDRs (or bare IPs, taken as `/32`/`/128`).
- IPv6-first; bracket-free (a CIDR, not a URL): `fd00:db8::/32`, not `[fd00:db8::]`.
- **Never-legitimate ranges cannot be re-enabled** — listing `169.254.0.0/16`,
  `127.0.0.0/8`, `::1/128`, multicast, or the unspecified address has no effect;
  the cloud-metadata IP and loopback stay hard-blocked.
- Unset or empty ⇒ deny every internal range (only public hosts dialled).
- A malformed entry logs a warning and **falls back to deny-all** (fail-closed),
  never dials wide.

### Example

Managed devices on `192.168.10.0/24` and a ULA `fd00:db8::/64`:

```bash
export MULTIVIEW_DEVICE_DIAL_ALLOW="192.168.10.0/24, fd00:db8::/64"
multiview run config.toml
```

For the OCI image, set it as a container env var (compose `environment:` /
`docker run -e`). It carries no secret, so it may live in plain deployment config.

## Verify

1. On start, the log emits
   `device-dial allowlist loaded from MULTIVIEW_DEVICE_DIAL_ALLOW entries=<n>`
   (or nothing when unset → deny-all).
2. A managed device on an allowlisted subnet reaches `ONLINE`
   (`GET /api/v1/devices` → its status), instead of riding `UNREACHABLE`.
3. Negative check: `POST /api/v1/cast/sessions` with `address` at an internal
   host **not** on the allowlist returns `422` and records no session; with
   `169.254.169.254:8009` it returns `422` regardless of the allowlist.

## Change / roll back

- **Widen/narrow:** edit the env var and restart the control plane (the policy is
  read at start, not hot-reloaded).
- **Roll back to secure default:** unset `MULTIVIEW_DEVICE_DIAL_ALLOW` and
  restart — every internal range is denied again.
- **Follow-up (config-as-code):** the permanent home is a `control.device_dial_allow`
  field in `ControlConfig`, to be added once the W026 `ControlConfig` edits land;
  when present it supersedes/merges with this env var.
