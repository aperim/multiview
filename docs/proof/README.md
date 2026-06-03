# Proof gallery — verification artifacts

These are committed, durable copies of the key verification frames captured while
building Mosaic out (the full set + MP4 clips live in the git-ignored
`demo-output/` working dir; this folder is the curated, reviewable subset). Every
image below was produced by **running the real software pipeline**
(`mosaic run --features ffmpeg,overlay <config>`) and extracting actual output
frames — not mock-ups.

To regenerate the full set locally: `cargo build -p mosaic-cli --features ffmpeg,overlay`
then run the configs under `/tmp/refdemo/` (or `.mosaic-build/render-gallery.sh`)
and extract frames from the produced `program.ts`.

| File | What it proves |
|------|----------------|
| `01-multiview-3x3-diverse-sources.png` | 9-tile multiview of **diverse real sources** (CNN HLS 29.97, ABC/SBS/10/9 DVB-T mpeg2 25, Red Bull HLS 30, Tears-of-Steel 24, Big Buck Bunny 24, synthetic) — all compositing together with per-tile labels + meters at the correct speed. |
| `02-multiview-2x2.png` | 2×2 multiview (CNN / ABC TV / synthetic tone / **a deliberately-missing source showing NO SIGNAL**) — per-tile label, LIVE/NO-SIGNAL state flag, and audio meter. |
| `03-timing-source-TC-1to1-at-2s.png` | The **timing fix**: a burned-timecode source tile reads `00:00:02.000` at output-time 2 s — i.e. source plays at exactly 1:1 with the output clock (the "ultra-fast-then-freeze" bug is gone). |
| `04-captions-live-hls-webvtt.png` | **Native HLS WebVTT captions decoded live** from the Apple bipbop stream — the real cue `English subtitle 1 -Unforced- (00:00:01.000)` burned into the tile (no sidecar file). |
| `05-captions-dvbsub-in-cue.png` / `06-captions-dvbsub-before-cue.png` | **Native DVB-sub (bitmap) captions** decoded from a broadcast MPEG-TS: the bitmap band is burned into the tile *during* its cue window (frame at t=2 s) and **absent before it** (t=0.4 s) — proving time-gated decode→burn-in. |
| `07-fault-badges-black-frozen-noaudio.png` | Per-tile **fault badges**: a black source → `BLACK`, a frozen source → `FROZEN`, a silent source → `NO AUDIO`, and a healthy source → no badge (no false positive), all driven by real content probes. |
| `08-analog-clock-t0.png` / `09-analog-clock-t10s.png` | **Analog clock face** (bezel ring + 12 ticks + hour/minute/second hands): the red second hand has swept from 12 toward 2 over 10 s, agreeing with the digital readout. |
| `10-wallclock-real-time-of-day.png` | **Wall-clock time-of-day read from the OS clock** (`17:xx:xx` = the actual render instant) with a `SYS locked` timing-reference badge, on a legible backing chip. |
| `web-01-dashboard-dark.png` … `web-08-mobile.png` | The **React management web UI**: dashboard (dark + light), layouts list, drag-and-drop layout editor (+ form), sources, outputs, settings, and a mobile view. Captured from the built SPA via headless Chromium. |

## Frame rate / smoothness — important note on the `seq_*` strips

The `seq_*.png` frame strips in the (git-ignored) `demo-output/` working dir were
extracted with `ffmpeg -vf fps=2` — **one frame every 0.5 s**. They prove a tile
is *advancing* (not frozen), but flipping through them looks like 2 fps because
that's the sampling rate of the strip, **not** the rendered output. Don't judge
smoothness from those.

The rendered output is genuine **25 fps**, verified objectively: rendering the
burned-timecode diag source (whose digits change every single frame, so a held
or duplicate frame is impossible to hide) and running `mpdecimate` over the
result yields **250 unique frames out of 250** for a 10 s clip — i.e. no
duplicate/held frames, smooth 25 fps. (A few-fps judder would show as ~25 unique
frames; it does not.)

- `11-consecutive-frames-40ms-apart.png` — 8 **consecutive** output frames (40 ms
  apart, 280 ms total) of the 5+1 layout; each frame differs from the last (slow
  news content, so the per-frame change is small but real).
- `12-multiview-1plus5-25fps.mp4` / `13-multiview-3x3-25fps.mp4` — the actual
  **25 fps playback** clips. Watch these for true motion, not the `seq_*` strips.

## Independent verification (re-run fresh, not asserted)

- `cargo fmt --check` · `clippy --workspace --all-targets -D warnings` · clippy under `ffmpeg,overlay` / `ffmpeg,test-fixtures` / `cuda` — all clean.
- `cargo test --workspace` (default GPU-free build): **1321 passed, 0 failed**.
- `cargo deny check`: advisories / bans / licenses / sources ok.

Each feature above also has a deterministic, failing-before/passing-after test in
its crate (e.g. `mosaic-framestore::sample_by_media_time` for the timing fix,
`mosaic-compositor::overlay_subpass` image-blit tests for the bitmap burn-in,
`mosaic-cli::overlays` per-tile burn-in/fault/clock tests).
