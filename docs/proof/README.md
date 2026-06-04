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

## Frame rate / smoothness — how this is **actually** verified (corrected)

> **Correction (2026-06-04).** Earlier revisions of this note claimed smoothness
> from `mpdecimate` ("250 unique frames out of 250") and from frame strips/
> screenshots. **Both methods are unreliable and the claim was withdrawn.**
> `mpdecimate` on a lossy MPEG-2/H.264 encode is fooled by quantization noise
> (every frame differs by a few LSB even if the *picture* is held), and any
> screenshot/hash comparison of frames carrying **animated overlays** (the clock,
> the audio meters) shows "advancing" even when the underlying video is frozen —
> the moving meter changes the hash. The `seq_*.png` strips in the git-ignored
> `demo-output/` dir are also a trap: they were sampled at `ffmpeg -vf fps=2`
> (one frame per 0.5 s), so flipping through them *looks* like 2 fps regardless of
> the real output rate. **Do not judge smoothness from strips, hashes, or
> `mpdecimate`.**

The output rate is a genuine, constant **25 fps** (250 frames / 10 s, `avg_frame_rate=25/1`,
no dropped/duplicated packets) — but *cadence* is not *smooth picture*. Picture
smoothness is verified the only reliable way: **content-aware, with overlays OFF,
against a ground-truth ffmpeg encode of the same source.** The gauge is the count
of frames whose inter-frame luma delta clears a motion floor:

```
ffmpeg -i program.ts -vf "tblend=all_mode=difference,signalstats,\
  metadata=print:key=lavfi.signalstats.YAVG:file=motion.txt" -f null -
# count frames with YAVG > 2  ==  frames that genuinely changed
```

Verified results (RELEASE build, this dev container, CPU-only software path):

| Check (overlays OFF) | Rendered (mosaic) | Ground truth (ffmpeg direct) |
|---|---|---|
| Single high-motion tile (Red Bull HLS 30) | **199 / 249** frames in motion, render real-time (10.4 s / 10 s) | 197 / 249 |
| 3-tile mixed-fps (29.97 + 25 + 30) | **191 / 249** in motion, render real-time (10.8 s / 10 s), frame-0 luma **87** (real content, not the cold slate ≈ 16) | — |
| 9-tile 720p (the 3×3 layout) | render **real-time (11.2 s / 10 s)**; centre tile faithfully tracks its source (`bcast_b`: static intro → motion later, **first-3s 2/75 → last-3s 75/75**, matching the source's own **0/75 → 36/75**) | source 36/209 |

Two real latent defects were found by this method and fixed (see
[ADR-0022](../decisions/ADR-0022.md) and the [ADR-0021 correction](../decisions/ADR-0021.md#correction-2026-06-04--the-verified-11-by-eye-above-was-overlay-masked)):

1. **Compositor real-time headroom** — the single-threaded software compositor was
   far slower than real time (≈ 0.9 s/tick in a debug build); under sustained load
   the output clock would sample **evicted** media-times from the 256-frame ring →
   frozen tiles (an invariant-#1 risk). Fixed by byte-identical row-band
   parallelism + an SSIM-gated colour-transfer LUT → **≈ 11 ms/tick @ 1080p × 9**,
   comfortably real-time (`benches/composite_realtime.rs` asserts ≤ 40 ms).
2. **Startup cold-slate** — the first ≈ 0.75 s held a cold NV12 slate before tiles
   primed. Fixed by a bounded first-frame prime (dead sources can't block startup);
   frame-0 luma now ≈ 90+ (real content), not ≈ 16.

The committed clips below are re-rendered from the **release** build and re-verified
content-aware:

- `11-consecutive-frames-40ms-apart.png` — 8 **consecutive** output frames (40 ms
  apart, 280 ms total) of the 5+1 layout tiled into one strip; each frame differs
  from the last. This is real consecutive motion, **not** the `seq_*` 0.5 s samples.
- `12-multiview-1plus5-25fps.mp4` / `13-multiview-3x3-25fps.mp4` /
  `14-multiview-2x2-25fps.mp4` — the actual **25 fps playback** clips. Watch these
  for true motion. (Overlays animate in these by design — to judge *picture*
  smoothness use the overlays-off content-aware numbers above, not these clips.)

## Independent verification (re-run fresh, not asserted)

- `cargo fmt --check` · `clippy --workspace --all-targets -D warnings` · clippy under `ffmpeg,overlay` / `ffmpeg,test-fixtures` / `cuda` — all clean.
- `cargo test --workspace` (default GPU-free build): **1321 passed, 0 failed**.
- `cargo deny check`: advisories / bans / licenses / sources ok.

Each feature above also has a deterministic, failing-before/passing-after test in
its crate (e.g. `mosaic-framestore::sample_by_media_time` for the timing fix,
`mosaic-compositor::overlay_subpass` image-blit tests for the bitmap burn-in,
`mosaic-cli::overlays` per-tile burn-in/fault/clock tests).
