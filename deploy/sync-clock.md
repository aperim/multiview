# Node clock discipline + acceptance soak (DEV-C4)

Display nodes in a **sync group** present frame-accurately against a common wall
timeline (ADR-M010). That only holds if each node's system clock is disciplined
to a shared reference. This guide covers the two supported disciplines, the
telemetry that proves they hold, and the acceptance-soak harness that certifies a
deployment.

> Clock discipline is a **node-OS** concern (`ptp4l`/`chronyd` run on the node,
> not inside Multiview). Multiview *reads* the disciplined clock and exports the
> servo offset/frequency so you can verify it — it never steps the system clock.

## Pass thresholds

The acceptance condition is the 99th-percentile of `|offset|` over a 24 h window,
per reference class (these are the constants in `multiview-telemetry::clock`, so
code and docs cannot drift):

| Reference | 99th-pct `|offset|` bound | Notes |
|-----------|---------------------------|-------|
| **PTP** (`ptp4l` + `phc2sys`) | **≤ 100 µs** | preferred; LAN hardware/software timestamping |
| **chrony/NTP** | **≤ 1 ms** | fallback on a non-PTP GbE switch |

The worst 1 % of samples may exceed the bound (it's a percentile, not a max) —
the analyzer tolerates the tail and fails only when the 99th-rank sample is over.

## Exported telemetry

Multiview exports these Prometheus gauges (scrape `/metrics`); the soak harness
reads them:

| Metric | Labels | Meaning |
|--------|--------|---------|
| `multiview_clock_servo_offset_nanoseconds` | `source="ptp"\|"system"` | disciplined-reference offset (ns) |
| `multiview_clock_servo_frequency_ppb` | `source` | servo frequency correction (ppb) |
| `multiview_audio_servo_resample_ppm` | `sink` | display-audio buffer-servo resample (ppm) |
| `multiview_audio_servo_fifo_fill_fraction` | `sink` | audio FIFO fill fraction |
| `multiview_audio_servo_skew_milliseconds` | `sink` | sample-vs-scanout skew (ms) |

## PTP (preferred) — `/etc/linuxptp/ptp4l.conf`

```ini
[global]
# Software timestamping is the portable default; set to hardware on a NIC + switch
# that support it for the tightest offsets.
time_stamping       software
# A single domain shared by every node in the sync group.
domainNumber        0
# IPv6-first transport (ADR-0042): PTP over UDP/IPv6.
network_transport   UDPv6
# Slave-only nodes never become master.
slaveOnly           1
logSyncInterval     -3
logMinDelayReqInterval -3

[eth0]
```

Discipline the *system* clock from the PTP hardware clock with `phc2sys`:

```ini
# /etc/linuxptp/phc2sys: -s <phc> -O 0 -w  (sync CLOCK_REALTIME to the PHC)
```

A `phc2sys` running with `-w` (wait for ptp4l) keeps `CLOCK_REALTIME` slaved; the
PHC = TAI, so the `[timing] ptp_utc_offset_s` (default 37) in the Multiview
config converts to UTC for the outbound epoch — see DEV-C1.

## chrony (fallback) — `/etc/chrony/chrony.conf`

```conf
# Two upstream sources minimum; iburst for fast initial sync.
pool        ntp.example.net iburst
# Step the clock only on the first few large corrections, then slew.
makestep    1.0 3
# Record drift so a restart re-converges quickly.
driftfile   /var/lib/chrony/drift
# IPv6-first: prefer the IPv6 addresses of the pool.
```

chrony on a non-PTP GbE switch typically holds well within the 1 ms bound.

## Running the acceptance soak

The harness (`scripts/soak-acceptance.sh`) drives a 2-node sync group, scrapes
each node's telemetry, injects the invariant-#1 chaos (kills PTP/WS mid-soak and
checks the output cadence never stalls — the nodes free-run on the held epoch),
and renders the PASS/FAIL verdict with the tested `cargo xtask soak-report`
analyzer.

**Verify the wiring (no hardware, CI-safe):**

```bash
scripts/soak-acceptance.sh --dry-run
```

This feeds bundled fixtures through `soak-report` and asserts a clean capture
passes while an offset breach and a cadence stall each fail.

**Real 24 h run (needs ≥2 nodes):**

```bash
scripts/soak-acceptance.sh \
  --controller 'http://[node-controller]:8080' \
  --group sg-livingroom \
  --node-metrics 'http://[node-a]:9090/metrics' \
  --node-metrics 'http://[node-b]:9090/metrics' \
  --chaos-hook 'ssh node-a systemctl stop ptp4l' \
  --frame-ocr-hook './capture-and-ocr.sh' \
  --duration 86400 --out soak-capture.json
```

`cargo xtask soak-report soak-capture.json` re-renders the verdict from a capture
at any time (exit 0 = PASS, 1 = FAIL).

### Hardware-gated

The **physical 24 h execution** — two real nodes on a non-PTP GbE switch, cameras
capturing the burnt-in frame counter, and OCR comparing cross-node skew — is the
hardware-validation step (it needs ≥ 2 physical nodes). The harness, the chaos
extension, and the verdict analyzer ship and are exercised in CI via `--dry-run`;
the camera capture + OCR is wired through `--frame-ocr-hook`. The burnt-in counter
itself comes from the sync-group **test-pattern** (DEV-C3), which the harness
starts over the API.
