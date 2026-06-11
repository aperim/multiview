# Deploying a Multiview display node (`multiview node`)

A **display node** (DEV-B5 / [ADR-0045]) turns a thin-client-class box (HP
t630 class, Raspberry Pi 4) into a network HDMI gateway: one supervised
ingest (RTSP/SRT/HLS/MPEG-TS) → hardware decode → full-canvas present → the
local DRM/KMS display head(s), with optional ELD-gated ALSA HDMI audio. It is
**the same binary and the same pipeline** as `multiview run` — the node
inherits the product's resilience doctrine (input pacer/jitter/supervised
reconnect, the framestore Live→Stale→Reconnecting→NoSignal ladder, last-good
then the configured local slate) by construction.

Until DEV-C2 lands the epoch + link-offset frame chooser, presentation rides
the display sink's repeat/drop reconciliation (≈ one duplicated/dropped frame
per ~16.7 s at a 60.000 Hz tick against 59.94 Hz glass); the node config's
`timing.link_offset_ms` is recorded and validated but not yet consumed.

## Building the binary

The node needs the `display-kms` (DRM/KMS scanout + ALSA HDMI audio) and
`ffmpeg` (libav\* ingest/decode) features, plus a hardware decode preset for
real deployments:

```sh
# AMD/Intel boxes (the t630 class — VAAPI decode):
cargo build --release -p multiview-cli --features display-kms,linux-vaapi

# NVIDIA boxes:
cargo build --release -p multiview-cli --features display-kms,nvidia

# Minimum (software decode only — fine for SD/low-fps feeds):
cargo build --release -p multiview-cli --features display-kms,ffmpeg
```

Build hosts need `libgbm-dev`, `libasound2-dev`, and the FFmpeg dev packages;
the binary dynamically links Mesa's `libgbm`, `libasound2`, and libav\*.

A binary **without** these features rejects `multiview node` with a clear
error naming the missing feature — it never half-runs.

## Configuration

See [`examples/display-node.toml`](../examples/display-node.toml) for the
annotated document: one `[ingest]`, one or more `[[displays]]` heads
(connector + optional `mode`/`forced_mode` + `audio`), the `on_loss` slate,
and the `[timing]`/`[hotplug]` knobs. Validate-by-running: the node validates
the document (and the lowered engine config) before touching any device.

Notes that matter in the field:

- **EDID-less heads** need `forced_mode` (CVT-RB timing) and have **no audio
  path** (no EDID ⇒ no ELD ⇒ HDMI audio is gated off; video runs).
- `connector = "auto"` is only accepted on single-head nodes.
- Frame rates are exact rationals (`"60000/1001"`), never floats.

## Bare metal (the blessed path)

Minimal Debian (or Raspberry Pi OS Lite); **no X11, no Wayland, no display
manager, no seatd/logind**. A process that opens `/dev/dri/cardN` while no
other KMS client exists becomes **DRM master implicitly** — no VT juggling.

```sh
# 1. Packages (Debian; amdgpu boxes need the firmware):
apt-get install --no-install-recommends \
    firmware-amd-graphics libgbm1 mesa-vulkan-drivers libasound2

# 2. The service user (unprivileged; device access via groups):
useradd --system --home-dir /var/lib/multiview --create-home multiview
usermod -aG video,render,audio multiview

# 3. Binary + config + unit:
install -m 0755 multiview /usr/local/bin/multiview
install -d -m 0750 -o root -g multiview /etc/multiview
install -m 0640 -o root -g multiview node.toml /etc/multiview/node.toml
install -m 0644 deploy/multiview-node.service /etc/systemd/system/

# 4. Nothing may fight for the console/GPU:
systemctl mask getty@tty1
#    (if an incumbent display manager exists, disable it: one DRM master per
#     card — the node REPLACES desktop stacks, it does not coexist with them)

# 5. Go:
systemctl daemon-reload
systemctl enable --now multiview-node
```

Kernel cmdline cosmetics (optional, for a clean boot-to-frame):
`quiet loglevel=3 consoleblank=0 vt.global_cursor_default=0`.

### Supervision semantics (what the unit relies on)

- **`Type=notify` + first-frame READY.** The node sends `READY=1` at the
  first output frame boundary — heads lit, output clock running — so
  `TimeoutStartSec` bounds the whole bring-up (probe → modeset → first
  frame), not just the exec.
- **Tick-gated watchdog.** With `WatchdogSec=` set, the node pings
  `WATCHDOG=1` at half the budget **only while the output clock advances**.
  A stalled clock withholds pings → systemd restarts the node. The watchdog
  enforces the output-never-falters invariant, not mere process existence.
- **Crash recovery.** fbcon does not hold DRM master; on a crash the
  kernel's fbdev-client restore brings the console back and `Restart=always`
  relights the head. `STOPPING=1` is sent on clean SIGTERM stops.

## Containers (supported-but-secondary)

### Rootful (full hotplug)

```sh
docker run -d --name multiview-node --restart unless-stopped \
  --device /dev/dri --device /dev/snd \
  --group-add video --group-add render --group-add audio \
  -v /etc/multiview/node.toml:/etc/multiview/node.toml:ro \
  ghcr.io/aperim/multiview:latest \
  multiview node /etc/multiview/node.toml
```

- **DRM master:** first open of the primary node becomes master implicitly
  when no other master exists — **no extra capabilities needed**. The host
  must not run any KMS client of its own (no display manager; a live master
  can never be displaced — `SET_MASTER` fails `EBUSY` regardless of
  capabilities).
- **Hotplug:** kernel kobject uevents are delivered to a netns owned by the
  initial user namespace — a normal rootful container **does receive kernel
  hotplug uevents**, and the node listens on the **kernel** netlink group
  directly (never the udevd-processed stream, which genuinely does not reach
  containers). Connector unplug/replug re-probes and re-lights automatically.
- **ALSA:** plain `/dev/snd` passthrough — the node is ALSA-direct, no
  PulseAudio/PipeWire sockets involved.

### Rootless (polling fallback)

A rootless container's network namespace is owned by its user namespace, so
**kernel uevents never arrive** (the socket binds fine — it just stays
silent). The node detects this via `/proc/self/uid_map` and automatically
falls back to `force_probe` connector polling at `hotplug.poll_secs`
(default 5 s; the kernel itself polls non-HPD connectors at 10 s). Everything
else works as rootful, provided the runtime maps the `/dev/dri` and
`/dev/snd` devices in with usable permissions.

`NOTIFY_SOCKET`/watchdog are systemd-only; in containers the notifier is
inert and the container runtime's restart policy is the supervisor.

## Bounded diagnostics

`multiview node node.toml --ticks 3600` (or `--duration 60`) runs a bounded
soak and prints the run report (frames/ticks emitted, cadence, whether the
output ever faltered) — the same numbers the 24 h t630 node soak collects.

[ADR-0045]: ../docs/decisions/ADR-0045.md
