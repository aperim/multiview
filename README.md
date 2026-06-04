# Multiview

**An efficient, hardware-accelerated, Rust live video multiview generator.** Ingest many live
sources, composite them into a templated multiview on the GPU, and serve the result robustly —
built to run great on **commodity hardware** with **bulletproof, never-falters output**.

> [!NOTE]
> **Status: early stage — design complete, implementation beginning.** This repository is the whole
> Multiview application. Its architecture, full API/UI design, 89 ADRs, and verification-hardened
> research are finished and pinned in [`docs/`](docs/). Implementation is just starting: the
> `crates/`, `xtask/`, and `web/` trees are an early scaffold — they compile
> (`cargo check`/`clippy`/`fmt` are green), but bodies are trait/type stubs being built out against
> the documented contracts. The current phase is the documentation pass; APIs and config shown below
> describe the target design. See [`ROADMAP.md`](ROADMAP.md) for the milestone plan and
> [`FEATURES.md`](FEATURES.md) for the capability/status matrix.

---

## What it does

Multiview is a headless, scriptable compositor/router. It samples many independent live inputs into
a fixed, templated canvas and encodes that canvas **once per rendition** — the encode-once-mux-many
design then fans the same stream to many transports (RTSP, HLS/LL-HLS, NDI, RTMP, SRT).

> **Status (early stage).** Today the engine ingests → composites → encodes → writes **HLS/file
> output**, driven by declarative config (TOML). The additional live output *servers* (RTSP/NDI/
> RTMP/SRT), the **web UI**, and the **OpenAPI-described control API** are built as libraries and on
> the near-term [roadmap](ROADMAP.md) — they are not yet wired into the daemon. See "Roadmap".

The two non-negotiable theses:

1. **Bulletproof, continuous output.** At every tick of a single fixed-cadence internal clock the
   output stage emits exactly one valid, correctly-timestamped frame (plus matching audio),
   *forever*, independent of any input. Inputs are *sampled*, never *pacing*. A dead camera shows a
   "no signal" card in its tile — it never freezes, stalls, or corrupts the multiview.
2. **Commodity hardware first.** The binding resource on an Intel iGPU, an AMD APU, a base Apple
   Silicon Mac, or an entry NVIDIA card is **memory bandwidth and fixed-function decode/encode**,
   not compositor math. Multiview decodes at display resolution, stays NV12 end-to-end, keeps frames
   on-device within a vendor island, and degrades tile-by-tile under load before the program output
   is ever touched. A 4-GPU server is the trivial case, not the target.

---

## Features

| Area | Capabilities |
|------|--------------|
| **Inputs** | RTSP, HLS/M3U, MPEG-TS, SRT, NDI (opt-in), RTMP, file, and synthetic test sources — with supervised reconnect, jitter buffers, and per-input timestamp normalization. |
| **GPU HAL** | Per-stage backend **auto-negotiation** (decode / composite / encode chosen independently) with a cost-model planner that prefers single-vendor **zero-copy islands** and costs every cross-vendor copy. Software is the universal fallback. |
| **Compositor** | A **custom GPU-native compositor** (not FFmpeg filters): scale + place + per-tile color convert + linear-light blend + overlays, fused into one pass. Owns all fit/cover/crop, gaps, borders, rounded corners. |
| **Layouts** | Declarative, **hot-reconfigurable** templates — named presets (`grid:2x2`, `grid:3x3`, `1+5`, `pip`), CSS-grid-like tracks with ASCII area maps, and absolute normalized rects for arbitrary PiP/overlap. |
| **Outputs** | RTSP, HLS, **Apple LL-HLS** (custom CMAF segmenter), NDI out, RTMP push, SRT push — via **encode-once-mux-many** fan-out. H.264 is the interop baseline; HEVC/AV1 are runtime-detected upgrades. |
| **Audio** | Per-input decode/resample/mix; clean **discrete per-input tracks** + a normalized program bus (EBU R128 / `loudnorm`); silence-fill on dropout so tracks never vanish; capability-aware routing per output. |
| **Subtitles** | CEA-608/708, DVB-sub, teletext, WebVTT/SRT/ASS ingest; **libass burn-in** (off the hot path) and format-aware discrete passthrough. |
| **Overlays** | Serializable layer stack — text, clocks, logos, tally borders, alert cards, audio meters — rendered input-decoupled so the alert path works even when every input and the GPU are gone. |
| **Web UI + API** | A single embedded React SPA + an **axum** REST API with **OpenAPI 3.1** (interactive Scalar docs), WebSocket/SSE realtime, auth + RBAC, and SQLite-backed config-as-code. |
| **Preview** | Sub-second **WHEP/WebRTC** preview + a cheap **MJPEG/JPEG** fallback, strictly isolated from the program path (preview can never back-pressure the engine). |

---

## Architecture

A layered Rust workspace with two planes: a **Tokio control/IO plane** for networking and the API,
and a dedicated-thread **data plane** for the codec/composite hot path. The protected output core
owns the clock and emits a frame every tick regardless of upstream state.

```mermaid
flowchart TB
    subgraph Ingest["Ingest — per-source supervised (multiview-input)"]
        SRCS["RTSP · HLS · MPEG-TS · SRT · RTMP · NDI · file · test"]
    end

    subgraph Data["Data plane — dedicated threads"]
        DEC["Decode (multiview-ffmpeg, HAL backends)"]
        FS["Per-tile last-good-frame store + state machine (multiview-framestore)"]
        COMP["Custom GPU compositor (multiview-compositor)"]
        ENC["Encode (HAL backends)"]
    end

    subgraph Core["Protected output core (multiview-engine)"]
        CLK(["Fixed-cadence output clock — PTS = f(tick)"])
        AUD["Audio mix + program bus (multiview-audio)"]
    end

    subgraph HAL["multiview-hal — capability detect + negotiation + cost model"]
        PLAN["Backend planner (admission + degradation)"]
    end

    subgraph Serve["Outputs — encode-once-mux-many (multiview-output)"]
        OUTS["RTSP · HLS/LL-HLS · NDI · RTMP · SRT"]
    end

    subgraph Mgmt["Control / IO plane (Tokio)"]
        API["REST + WS + SSE API (multiview-control, axum + OpenAPI)"]
        WEB["Embedded React SPA (web/)"]
        PREV["Preview taps — WHEP / MJPEG (multiview-preview)"]
        TEL["Telemetry + health (multiview-telemetry)"]
    end

    SRCS --> DEC --> FS --> COMP --> ENC --> OUTS
    CLK -- "pull 1 frame/tick" --> COMP
    CLK -- "pull samples/tick" --> AUD --> ENC
    PLAN --> DEC & COMP & ENC
    API --> PLAN
    WEB --> API
    COMP -. tap .-> PREV
    TEL -. observes .-> Data
```

See the [Core Engine brief](docs/research/core-engine.md) and
[Resilience & A/V brief](docs/research/resilience-and-av.md) for the full data flow, and the
[canonical conventions](docs/architecture/conventions.md) for the authoritative crate map, feature
flags, and invariants.

---

## Quick start (Docker Compose)

The fastest way to see Multiview running. It brings up the engine plus a small companion
container that publishes a **synthetic** `testsrc2` + `sine` RTSP feed (no real or private
sources), composites a 2×2 canvas, encodes it, and serves the result as HLS.

```bash
git clone https://github.com/aperim/multiview.git
cd multiview

# Pulls ghcr.io/aperim/multiview:latest + a MediaMTX testsrc companion + an
# nginx that serves the HLS output.
docker compose -f deploy/compose.yaml up -d

# Then open the multiview in a player (VLC / ffplay):
#   vlc http://localhost:8888/multiview.m3u8
docker compose -f deploy/compose.yaml logs -f multiview
```

The quick-start config is [`deploy/config/multiview.toml`](deploy/config/multiview.toml) — one tile
reads the companion's `rtsp://testsrc:8554/test`, the other three are built-in synthetic test
patterns. Edit it and re-run `up -d` to point a tile at your own source. Tear down with
`docker compose -f deploy/compose.yaml down -v`.

> [!NOTE]
> The default LGPL-clean image encodes **mpeg2video** — open the HLS in **VLC**/ffplay. For
> browser-friendly **H.264**, use the `-gpl` image and set `codec = "h264"` (H.264/H.265 make the
> build GPL). A **web UI + control API** (the embedded SPA and axum control plane are built) and
> live **RTSP/NDI/RTMP output servers** are on the [roadmap](ROADMAP.md) — today the engine
> composites, encodes, and writes HLS/file output to disk (no network listener yet).

### GPU one-liners

```bash
# NVIDIA (NVDEC/NVENC/CUDA): needs the NVIDIA driver + Container Toolkit on the host.
docker compose -f deploy/compose.yaml -f deploy/compose.gpu-nvidia.yaml up -d

# Intel/AMD VAAPI: passes through /dev/dri; set RENDER_GID to your host's render group id.
RENDER_GID=$(getent group render | cut -d: -f3) \
  docker compose -f deploy/compose.yaml -f deploy/compose.gpu-vaapi.yaml up -d
```

---

## Install

### Container image (GHCR)

```bash
# LGPL-clean default image (software + VAAPI). Encodes the canvas with LGPL mpeg2video.
docker pull ghcr.io/aperim/multiview:latest

# NVIDIA variant (NVDEC/NVENC/CUDA).
docker pull ghcr.io/aperim/multiview:latest-nvidia
```

Images are multi-arch (`linux/amd64` + `linux/arm64`), built on native runners, and published with
SLSA build-provenance attestations + keyless cosign signatures.

### Prebuilt binaries (GitHub Releases)

Each tagged release attaches a `tar.gz` (+ `.sha256`) per platform on the
[Releases page](https://github.com/aperim/multiview/releases):

| Platform | Asset target |
|----------|--------------|
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| macOS Apple Silicon | `aarch64-apple-darwin` (signed + notarized) |
| macOS Intel | `x86_64-apple-darwin` (signed + notarized) |

> Two separate macOS binaries are shipped (not a universal2 `lipo`): Homebrew's FFmpeg bottle is
> arm64-only on the build runner, so a true universal2 link isn't yet available.

### Runtime requirement: FFmpeg 7.x

The released binaries link **FFmpeg / libav 7.x dynamically** — the `libavcodec.so.61` soname
family. You must have a matching FFmpeg 7.x on the host:

- **macOS:** `brew install ffmpeg` (Homebrew ships 7.x).
- **Linux:** distro libav 7.x — e.g. **Debian trixie** ships FFmpeg 7.1 (`libavcodec61` …). On
  **Ubuntu 24.04** apt ships FFmpeg **6.1**, so install 7.x from a maintained PPA
  (`ppa:ubuntuhandbook1/ffmpeg7`) or use the container image instead.
- Verify with `pkg-config --modversion libavcodec` → expect `61.x`.

The default container image bundles the correct FFmpeg 7.x runtime, so no host FFmpeg is needed when
running via Docker.

### Build from source

```bash
# Default build: pure-Rust trait/type layer, no native GPU deps, LGPL-clean.
cargo build

# Platform umbrella presets (defined in multiview-cli):
cargo build --features ffmpeg                  # real libav* pipeline, LGPL mpeg2video
cargo build --features nvidia                  # cuda + ffmpeg + wgpu (NVENC/NVDEC/CUDA)
cargo build --features apple                   # videotoolbox + metal + ffmpeg (macOS)
cargo build --features linux-vaapi             # vaapi + qsv + ffmpeg + wgpu (Intel/AMD)
cargo build --features full                    # everything non-GPL
cargo build --features ffmpeg,gpl-codecs       # adds x264/x265 — relicenses the build GPL
```

### Run a 2×2 multiview

Multiview is configured by a declarative TOML document — canvas, layout, sources, cells, overlays,
and outputs. Self-contained examples live in [`examples/`](examples/) (all built-in `test` sources,
no network needed).

```bash
# Validate a config without starting the pipeline
multiview validate examples/2x2.toml

# Run it (real libav* pipeline). Bound the run with --duration / --ticks, or
# --headless for the GPU/FFmpeg-free software output-clock smoke.
multiview run examples/2x2.toml --duration 10
multiview run examples/2x2.toml --headless --ticks 300
```

A minimal 2×2 canvas drawing from four synthetic sources (see [`examples/2x2.toml`](examples/2x2.toml)):

```toml
schema_version = 1

[canvas]
width = 1920
height = 1080
fps = "30000/1001"          # exact rational string — never a float fps
pixel_format = "nv12"
background = "#101014"

[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr", "1fr"]
rows = ["1fr", "1fr"]
gap = 8
areas = ["a b", "c d"]

[[sources]]
id = "in_a"
kind = "test"               # built-in synthetic pattern; swap to rtsp/hls/ts/srt/file/ndi
# ... in_b / in_c / in_d likewise ...

[[cells]]
id = "cell_a"
area = "a"
fit = "contain"
[cells.source]
input_id = "in_a"
# ... cell_b / cell_c / cell_d likewise ...

# Encode once, fan out to many transports (invariant #7).
[[outputs]]
kind = "rtsp_server"
mount = "/multiview"
codec = "h264"
latency_profile = "low_latency"
```

Once the control plane is wired, the web UI and OpenAPI docs are served from the same process on
`:8080` (Scalar try-it-out at `/docs`, spec at `/api/v1/openapi.json`).

> Synthetic and public test-stream recipes — reproducible `lavfi testsrc2`/`sine` feeds, Big Buck
> Bunny, and a deliberately diverse "gotcha" matrix (mixed fps, codecs, untagged color, subtitles) —
> are cataloged in **[docs/reference/example-streams.md](docs/reference/example-streams.md)**.

---

## Platform support

| Platform | Decode | Composite | Encode | Notes |
|----------|--------|-----------|--------|-------|
| **Linux / NVIDIA** | NVDEC | Custom CUDA | NVENC | Zero-copy NVDEC→CUDA→NVENC island; via NVIDIA Container Toolkit. |
| **Linux / Intel·AMD** | VAAPI / QSV | Vulkan·wgpu·libplacebo | VAAPI / QSV | dma-buf zero-copy where the driver allows; `/dev/dri` passthrough. |
| **macOS (Apple Silicon + Intel)** | VideoToolbox | Metal | VideoToolbox | Native universal2 build; zero-copy VT→Metal→VT island. |
| **Any (software fallback)** | libav / dav1d | wgpu / CPU | x264 / SVT-AV1 | Universal fallback tier; the GPU-free CI path. |

**No Windows.** Targets are Linux (x86_64 + aarch64, containerized) and macOS (native). Edition
Rust 2021, pinned via `rust-toolchain.toml`; MSRV documented at release.

---

## Documentation

| Section | What's there |
|---------|--------------|
| **[docs/architecture](docs/architecture/)** | The [canonical conventions](docs/architecture/conventions.md) — **source of truth** for crate names, API paths, feature flags, invariants, and licensing. |
| **[docs/decisions](docs/decisions/)** | 72 Architecture Decision Records ([index](docs/decisions/README.md)) capturing every load-bearing choice. |
| **[docs/research](docs/research/)** | Deep, verification-hardened design briefs ([index](docs/research/README.md)) — [core engine](docs/research/core-engine.md), [resilience & A/V](docs/research/resilience-and-av.md), [efficiency](docs/research/efficiency.md), [color](docs/research/color-management.md), [web/API stack](docs/research/web-api-stack.md), and more. |
| **API** | REST + realtime conventions are pinned in [conventions.md §6](docs/architecture/conventions.md); the live OpenAPI 3.1 spec is served at `/api/v1/openapi.json` with interactive Scalar docs at `/docs`. |
| **[docs/reference](docs/reference/)** | [Example & test streams](docs/reference/example-streams.md) and the [bibliography](docs/reference/bibliography.md). |

---

## Licensing

Project code is dual-licensed **MIT OR Apache-2.0** — use either at your option.

The **default build is LGPL-clean and redistributable**: FFmpeg is linked LGPL, NVENC/NVDEC use the
MIT `nv-codec-headers` (no `--enable-gpl`, no `--enable-nonfree`), and all scaling/compositing is
done in-house (no libnpp, no x264/x265). Two capabilities are strictly opt-in:

| Feature | Effect |
|---------|--------|
| `gpl-codecs` | Pulls in x264/x265 → the resulting build is **GPL**. Off by default. |
| `ndi` | Uses the **proprietary** NDI SDK (royalty-free, runtime-loaded, never vendored). Carries the NDI EULA and **mandatory attribution** — "NDI® is a registered trademark of Vizrt NDI AB." Off by default. |

The published container images and release binaries are built **LGPL-clean** (no `gpl-codecs`): the
canvas is encoded with LGPL `mpeg2video`, so a config naming `h264` falls back to MPEG-2. A separate
**`-gpl` image tag** (built with `--build-arg CARGO_FEATURES=ffmpeg,linux-vaapi,gpl-codecs`) provides
true x264/x265 H.264/HEVC output and is, as a whole, **GPL-licensed** — use it only if that license
is acceptable for your deployment.

Codec **patent** licensing (H.264/HEVC/AAC pools) is a separate question from software copyright and
may apply to your outputs regardless of build flags. CI gates licenses and advisories with
`cargo-deny`. See [conventions.md §7](docs/architecture/conventions.md) and
[ADR-0012](docs/decisions/ADR-0012.md) for the full licensing model.
