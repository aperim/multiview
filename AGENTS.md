# AGENTS.md — Agent Contract for Mosaic

This is the **canonical, tool-agnostic guide** for any AI/automation agent (or new human
contributor) working in the Mosaic repository. It tells you what the project is, how it is laid
out, how to build and test it, and the invariants you **must not** violate.

> **Source of truth.** Naming, crate map, feature flags, invariants, API conventions, and licensing
> are pinned in [`docs/architecture/conventions.md`](docs/architecture/conventions.md). Where any
> brief, ADR, or this file disagrees with that document, **conventions wins**, and the Rust code is
> the ultimate authority. Read conventions first; treat the rest as elaboration.
>
> **Companion file.** [`CLAUDE.md`](CLAUDE.md) is the Claude-Code-specific companion (host
> environment, MCP servers, credential retrieval). This file is the generic contract; do not
> duplicate CLAUDE.md content here.

---

## Engineering guardrails (non-negotiable)

Full standard: [`docs/development/agent-guardrails.md`](docs/development/agent-guardrails.md). Conventions/naming source of truth: [`docs/architecture/conventions.md`](docs/architecture/conventions.md). All three pillars are blocking CI.

**1. Absolute typing — no untyped, no escape hatches.**
- Rust: lint policy is centralized in root `[workspace.lints]`; every crate uses `[lints]` `workspace = true`. **Denied in non-test code:** `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`, `get_unwrap`, `indexing_slicing`, `as_conversions`, `dbg_macro`, `print_stdout/stderr`, `str_to_string`, `exit`, `mem_forget`. `unsafe_code = forbid` (FFI crates: `deny` + `// SAFETY:`). Prefer `?`/`match`/`unwrap_or`/`let-else` over unwrap/expect. Use newtype+`TryFrom`, typestate, `#[non_exhaustive]`, exhaustive `match`. **Ban `dyn Any`.** Tests relaxed via `clippy.toml allow-*-in-tests`; **every `tests/` file needs `#![allow(clippy::unwrap_used, …)]`** (those options don't cover integration tests).
- TS: `tsconfig` `strict` **+** `noUncheckedIndexedAccess` + `exactOptionalPropertyTypes` (+ override/returns/switch flags). ESLint `strictTypeChecked` (type-aware) bans `any` + `no-unsafe-*`; `ban-ts-comment` (no `@ts-ignore`/`@ts-nocheck`, `@ts-expect-error` allow-with-description) and `no-non-null-assertion` (no `!`).
- Gates: `cargo clippy --all-targets --all-features -- -D warnings`, `tsc --noEmit`, `eslint . --max-warnings=0`.

**2. TDD-first with REAL tests.** Write the failing test FIRST; run it and paste the failing output; **commit failing tests separately**; then implement to green WITHOUT touching tests. **NEVER weaken/delete/skip/`#[ignore]` a test, weaken an assertion, or edit code-under-test to fit a weak test — STOP and ask a human.** No tautological/assertion-free tests. Coverage is a floor; **mutation score is the target**: `cargo mutants --in-diff` on PRs (a MISSED mutant in changed code fails the PR), full run nightly. Property tests required for pure/stateful logic (`proptest`/`proptest-state-machine`, commit `proptest-regressions/`; `fast-check` for TS). Keep a held-out acceptance suite the author never sees.

**3. Adversarial cross-vendor review (required).** Code authored by one vendor is reviewed by a **different** vendor (Claude ↔ Codex ↔ Gemini) in a **fresh context** seeing only diff + spec + checklist. Reviewer scope: correctness/security/spec/guardrail defects only. Reviewer checks the typing + TDD rules above and that no test was weakened. Unanimous approval is a yellow flag. **A human is always the final approver.**

**Baseline:** explore→plan→implement→commit; minimal in-scope diffs with a stated out-of-scope boundary; **no silent suppression** (any `#[allow]`/`eslint-disable`/`.skip` needs an inline justification + review; fix root cause); show evidence not assertions; propagate errors with `?`, never swallow; build `--locked` + `npm ci`, commit lockfiles; secrets via 1Password (`op read`→`chmod 600`→`rm -f`), gitleaks pre-commit + CI; `cargo deny check`; Conventional Commits + `Co-Authored-By:` trailer; ADRs in `docs/decisions/` for non-trivial decisions; **no copying proprietary/competitor features, designs, or trademarked terms — build from open standards + original work, keep docs vendor-neutral** ([CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)). Definition of Done in the full doc.

---

## 1. What Mosaic is

Mosaic is an efficient, hardware-accelerated, **Rust live video mosaic generator**. It ingests many
live sources (RTSP, HLS/M3U, MPEG-TS, SRT, RTMP, NDI, file, test), composites them into a templated
mosaic on the **GPU**, and serves the result robustly (RTSP, HLS/LL-HLS, NDI, RTMP/SRT push).

- **Binary / daemon:** `mosaic`
- **Design goal:** great on **commodity hardware**, with **bulletproof, never-faltering output**.
- **Platforms:** Linux (x86_64 + aarch64; NVIDIA via Container Toolkit, Intel/AMD via VAAPI) and
 macOS (Apple Silicon + Intel, native). **No Windows.**
- **Edition / toolchain:** Rust **2021**, stable, pinned via `rust-toolchain.toml`.
- **License:** project code is dual **MIT OR Apache-2.0**. See §7 for the build-profile licensing model.

The engine is a **hybrid**: FFmpeg/libav (via `rsmpeg`) for demux/decode/encode where libav is
strongest, plus **custom Rust + GPU-native code** for the compositor and the serving/output side.
Deep rationale lives in the briefs (§9) and the ADRs (`docs/decisions/`).

---

## 2. Repository layout

```
mosaic/
├── Cargo.toml # workspace (resolver = "2")
├── rust-toolchain.toml rustfmt.toml .editorconfig deny.toml clippy.toml
├── LICENSE-MIT LICENSE-APACHE README.md CLAUDE.md AGENTS.md CONTRIBUTING.md SECURITY.md
├── crates/ # all Rust crates, prefixed mosaic-* (see §3)
├── web/ # management SPA (React 19 + TS + Vite)
├── docs/ # architecture, decisions (ADRs), research briefs, reference, ops
├── examples/ # example mosaic configs + layout templates
├── deploy/ # Dockerfile, compose, container assets
├── xtask/ # dev automation (cargo xtask ...)
└── .github/workflows/ # CI
```

> `.mosaic-build/` is a transient, git-ignored working dir used during doc/scaffold generation. It
> is **not** part of the product — never reference it from product code or shipped docs.

The repo is at an **early stage**: the workspace scaffold compiles (`cargo check`/`clippy`/`fmt`
are green), but most crate bodies are trait/type stubs. Build them out against the documented
contracts; conform to this layout and the canonical crate map below.

---

## 3. Canonical crate map

All crates are prefixed `mosaic-` and live under `crates/`. The library target for `mosaic-<area>`
is `mosaic_<area>` (snake, automatic). **All hardware/FFI/GPU code sits behind off-by-default Cargo
features** so the default `cargo check` builds the pure-Rust trait/type layer with no native deps.

| Crate | Responsibility |
|-------|----------------|
| `mosaic-core` | Shared types & traits: `Frame`, `PixelFormat` (NV12 canonical), `ColorInfo` (4 axes), clock/`MediaTime`, layout/template model, error taxonomy, stage traits (`Source`, `Sink`, `Decoder`, `Encoder`, `Compositor`, `Backend`). **No FFI.** |
| `mosaic-hal` | Hardware capability detection, backend registry, per-stage negotiation + cost model / planner (admission + degradation inputs). |
| `mosaic-ffmpeg` | Safe RAII wrappers over libav* (demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe transfer/map). |
| `mosaic-compositor` | Custom GPU compositor: scale + place + per-tile color convert + linear-light blend + overlay compositing. wgpu baseline; vendor fast paths (CUDA/Metal). |
| `mosaic-framestore` | Per-tile last-good-frame stores (lock-free triple-buffer) + tile state machine (LIVE/STALE/RECONNECTING/NO_SIGNAL). |
| `mosaic-audio` | Per-input audio decode/resample/mix/route (program bus + discrete tracks) + EBU R128 metering. |
| `mosaic-overlay` | Overlay layers + text rendering + subtitle ingest/render (libass) and passthrough. |
| `mosaic-input` | Ingest sources, the input pacer, jitter buffers, timestamp normalization, supervised reconnect. |
| `mosaic-output` | Output sinks/servers (RTSP, HLS/LL-HLS, NDI out, RTMP/SRT push); encode-once-mux-many fan-out. |
| `mosaic-engine` | The protected output core: fixed-cadence output clock, compositor drive, supervisor/actors, hot-reconfiguration, admission/degradation control loop. |
| `mosaic-config` | Config & template schema (serde), validation, config-as-code import/export. |
| `mosaic-events` | Shared realtime event types + versioned envelope (engine, control, clients). |
| `mosaic-control` | The axum REST + WebSocket + SSE API: OpenAPI (utoipa + Scalar), auth, SQLite (sqlx), command-bus shell, embedded SPA serving. |
| `mosaic-preview` | Preview taps (input/program/output), preview encoder pool, WHEP/MJPEG/snapshot endpoints. **Strictly isolated** from the program path. |
| `mosaic-telemetry` | `tracing` + Prometheus metrics + health (`/livez`, `/readyz`). |
| `mosaic-cli` | The `mosaic` binary: wires engine + control plane; config load; run/validate subcommands. |
| `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint, package). |

**Dependency direction (no cycles):** `core` ← everything; leaf crates depend on `core` (+ `hal`,
`ffmpeg`, `events` as needed); `engine` depends on the media crates; `control`/`preview` depend on
`engine` + `events`; `cli` depends on all.

> The research brief uses some earlier crate names (`mosaic-sys`, `mosaic-io`, `mosaic-server`,
> `mosaic-control` with `/api/v1`). **The table above is canonical.** Prefer it.

---

## 4. Build, test & lint commands

Default features build a **pure-Rust, LGPL-clean, no-native-deps** check, so CI is green without
GPUs or libav present. Hardware paths are opt-in (§5).

```bash
# Fast, dependency-light check (the default CI gate — no native deps)
cargo check --workspace
cargo build --workspace

# Lint & format (CI runs clippy with -D warnings)
cargo fmt --all
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# Tests (software/CPU backends run everywhere, including GPU-free CI)
cargo test --workspace

# License / advisory gate
cargo deny check

# Dev automation (web build, codegen, packaging, etc.)
cargo xtask --help

# Build with a hardware preset (see §5) — requires the relevant SDK/toolchain
cargo build -p mosaic-cli --features nvidia # CUDA + FFmpeg + wgpu
cargo build -p mosaic-cli --features apple # VideoToolbox + Metal + FFmpeg
cargo build -p mosaic-cli --features linux-vaapi # VAAPI + QSV + FFmpeg + wgpu

# Run / validate config
cargo run -p mosaic-cli -- run --config examples/mosaic.toml
cargo run -p mosaic-cli -- validate --config examples/mosaic.toml
```

**CI tiering:** the software/CPU backend full pipeline + golden-frame + mocks run on free GitHub
runners. Real GPU decode/composite/encode + SSIM/PSNR run on GPU-tagged self-hosted runners. Write
new code so the **software path keeps CI green without a GPU**.

---

## 5. Feature-flag taxonomy

Defaults build the pure-Rust, LGPL-clean tier. Everything hardware/native is additive and
off-by-default; **enabling a feature must never change the public API**.

- **Codec backends (per-stage, auto-negotiated at runtime):** `cuda` (NVDEC/NVENC),
 `videotoolbox`, `vaapi`, `qsv`, `software` (always on).
- **Compositor backends:** `wgpu` (default, cross-platform), `metal`, `cuda`.
- **Media engine:** `ffmpeg` (links libav for demux/decode/encode).
- **Codecs licensing:** `gpl-codecs` (x264/x265 → makes the build **GPL**; off by default).
- **NDI:** `ndi` (proprietary SDK; off by default, runtime-loaded; see §7).
- **Subtitles:** `libass`.
- **Web/API:** `openapi` (default), `embed-web` (embed the SPA), `webrtc` (WHEP preview).
- **Umbrella presets (in `mosaic-cli`):** `nvidia` = cuda+ffmpeg+wgpu; `apple` =
 videotoolbox+metal+ffmpeg; `linux-vaapi` = vaapi+qsv+ffmpeg+wgpu; `full` = everything non-GPL.

---

## 6. Invariants you MUST respect

These are load-bearing. Do not "optimize" or refactor them away; a change that breaks one is a
regression even if tests pass. Full statements: [conventions §5](docs/architecture/conventions.md).

1. **Output-clock invariant (bulletproof output).** A single fixed-cadence monotonic clock drives
 the output; at every tick the output emits exactly one valid, correctly-timestamped frame (+
 matching audio), **forever**, independent of any input. Output PTS = `f(tick)`. Inputs are
 *sampled*, never *pacing*. See [ADR-R001](docs/decisions/ADR-R001.md), [ADR-T001](docs/decisions/ADR-T001.md).
2. **Per-tile last-good-frame + state machine.** Inputs write into lock-free single-slot stores; the
 compositor always reads the latest (or a placeholder card) and **never blocks**. Tiles ride
 LIVE→STALE→RECONNECTING→NO_SIGNAL. ([ADR-T002](docs/decisions/ADR-T002.md))
3. **Deadline-driven compositor.** Never wait-for-all-inputs; one stalled source must never freeze
 the mosaic. ([ADR-0013](docs/decisions/ADR-0013.md))
4. **Unified timing model.** Per-input PTS is normalized (unwrap 33-bit wrap, genpts fallback,
 monotonic guard, discontinuity re-anchor) onto one internal ns timeline; the output re-stamps all
 PTS/DTS from the tick counter. NTSC `1001` rates are exact rationals/ns — **never float fps**.
 ([ADR-T003](docs/decisions/ADR-T003.md))
5. **HLS ingest pacing.** Live/VOD-as-live inputs are paced to wall-clock by PTS (custom pacer);
 `-re` is for files, not live ingest. ([ADR-T004](docs/decisions/ADR-T004.md))
6. **NV12-throughout.** Frames stay NV12 (1.5 B/px); never materialize RGBA per tile. YUV→RGB
 happens in-shader at tile size. ([ADR-E002](docs/decisions/ADR-E002.md))
7. **Decode-at-display-resolution.** Decode each source near its displayed size where the backend
 supports it; budget decode in megapixels/sec. ([ADR-E001](docs/decisions/ADR-E001.md))
8. **Encode-once-mux-many.** Composite once, encode the canvas once per rendition, fan the *same*
 packets to all transports. Separate encode only when codec/res/bitrate differ.
 ([ADR-E003](docs/decisions/ADR-E003.md), [ADR-E004](docs/decisions/ADR-E004.md), [ADR-0014](docs/decisions/ADR-0014.md))
9. **Color pipeline order — never reorder.** detect 4 axes (untagged-default policy that matches
 players, not swscale) → range-expand → YUV→RGB matrix → linearize (EOTF) → primaries convert in
 linear → scale + premultiplied-alpha blend in linear → OETF → RGB→YUV + range compress → **tag
 the output** → verify with ffprobe. ([ADR-C001](docs/decisions/ADR-C001.md)–[ADR-C006](docs/decisions/ADR-C006.md))
10. **Zero-copy islands per vendor.** Keep decode→composite→encode on one physical device where
 possible. Cross-vendor on-GPU zero-copy **does not exist on desktop** — insert exactly one
 explicit, costed copy at any vendor / NDI / CPU boundary. ([ADR-0004](docs/decisions/ADR-0004.md))
11. **Resource-adaptive degradation.** A closed control loop (sense→estimate→plan→apply, with
 hysteresis) sheds load **tile-by-tile, cheapest-impact-first, BEFORE** the program output is
 touched. Bounded queues **drop, never grow**. ([ADR-E007](docs/decisions/ADR-E007.md))
12. **Isolation (no back-pressure into the engine).** The control plane, preview, and realtime
 layers are best-effort and **physically incapable of back-pressuring the engine**
 (watch/broadcast channels; bounded drop-oldest queues; the engine never awaits a client). A CI
 chaos gate enforces this. ([ADR-R002](docs/decisions/ADR-R002.md), [ADR-P001](docs/decisions/ADR-P001.md), [ADR-RT004](docs/decisions/ADR-RT004.md))
13. **Live-apply classification.** Every management change is classified **Class-1** (hot/seamless
 at a frame boundary) vs **Class-2** (controlled reset via make-before-break parallel-output
 migration); the API surfaces which **before** applying. ([ADR-M005](docs/decisions/ADR-M005.md), [ADR-R004](docs/decisions/ADR-R004.md))

### Concurrency rules (don't break these)

- **Two planes.** A **data plane** of dedicated OS threads runs the codec/composite/encode hot path
 (long synchronous CUDA/VideoToolbox/libav calls **must never** run on Tokio workers). A
 **control/IO plane** uses Tokio for networking and the HTTP/WS API. ([ADR-0009](docs/decisions/ADR-0009.md))
- **One actor per source**, feeding a small **bounded, drop-oldest** queue. Per-source isolation
 prevents head-of-line blocking; `av_read_frame` on one source never blocks the composite loop.
- **Channels carry ref-counted pooled frame handles, never pixels.** Buffers come from per-device
 pools allocated at start, returned via `Drop` — never per-frame allocation.

---

## 7. Licensing model (build profiles)

- **Project code:** dual **MIT OR Apache-2.0**.
- **Default build = LGPL-clean & redistributable.** FFmpeg linked LGPL; NVENC/NVDEC via
 `nv-codec-headers` (MIT) need neither `--enable-gpl` nor `--enable-nonfree`; **no** libnpp /
 x264 / x265 in the default build (compositing/scaling done in-house with `scale_cuda`, not
 `scale_npp`).
- **`gpl-codecs` feature** pulls x264/x265 → the resulting build is **GPL**. Opt-in only.
- **NDI** SDK is **proprietary** (royalty-free, attribution required, redistribution restricted). It
 is **never vendored**; the `ndi` feature uses a runtime dynamic-load path (`NDIlib_v6_load()`) and
 the SDK/runtime is the user's responsibility. Carry the EULA + the **"NDI® is a registered
 trademark of Vizrt NDI AB"** attribution and a link to ndi.video.
- CI uses **`cargo-deny`** (`deny.toml`) to gate licenses and advisories; the effective license is
 reported **per built artifact**. Verify FFmpeg with `ffmpeg -buildconf` (no
 `--enable-gpl`/`--enable-nonfree`/`--enable-libnpp`). See [ADR-0012](docs/decisions/ADR-0012.md).

---

## 8. API, realtime & frontend conventions

When touching the control plane or web app, follow these (full detail in
[conventions §6 & §8](docs/architecture/conventions.md)):

- **REST base path:** `/api/v1`. Commands/CRUD only. Long-running ops return `202 Accepted` + an
 operation id; the result arrives on the realtime stream.
- **Error model:** RFC 9457 `application/problem+json`.
- **Concurrency / idempotency:** per-resource version → `ETag`/`If-Match`, `412` on mismatch;
 `Idempotency-Key` on start/stop/swap.
- **OpenAPI:** 3.1 via **utoipa**; interactive **Scalar** docs at `/docs`; spec at
 `/api/v1/openapi.json`. ([ADR-W002](docs/decisions/ADR-W002.md))
- **Realtime:** **WebSocket** primary at `/api/v1/ws` (versioned envelope, snapshot+delta,
 subscriptions, resume via `seq`); **SSE** fallback at `/api/v1/events`; documented with
 **AsyncAPI** at `/docs/events`. High-rate audio meters are conflated (~10–30 Hz).
 ([ADR-RT001](docs/decisions/ADR-RT001.md)–[ADR-RT006](docs/decisions/ADR-RT006.md))
- **Auth:** UI = `tower-sessions` cookie + CSRF; machine = hashed API keys (Bearer). RBAC
 admin/operator/viewer via `axum-login`. **Per-object authorization on every resource id** (BOLA is
 the #1 risk). ([ADR-W005](docs/decisions/ADR-W005.md))
- **Preview:** WHEP (sub-second focus) + MJPEG/JPEG (cheap grid); gated by short-lived signed
 tokens; auto-stop with no subscribers. Preview taps the **real** encoded bitstream where possible.
 ([ADR-P002](docs/decisions/ADR-P002.md), [ADR-P005](docs/decisions/ADR-P005.md))
- **Frontend stack:** React 19 + TypeScript + Vite; **shadcn/ui** (Radix + Tailwind v4); **TanStack
 Query** (server state) + **TanStack Table** (lists); layout editor = **react-konva** + **dnd-kit**;
 API client generated from the OpenAPI spec; WCAG 2.1 AA. Built into the `mosaic` binary via
 `rust-embed`. ([ADR-W003](docs/decisions/ADR-W003.md), [ADR-W004](docs/decisions/ADR-W004.md), [ADR-W007](docs/decisions/ADR-W007.md))

> **Management completeness is a contract.** Every controllable engine parameter must be reachable
> through a versioned API resource **and** a named UI control. See the capability matrix brief (§9).

---

## 9. Where to find the deep design docs

- **Architecture conventions (SOURCE OF TRUTH):**
 [`docs/architecture/conventions.md`](docs/architecture/conventions.md)
- **Decisions (ADRs):** [`docs/decisions/`](docs/decisions/) — see
 [`docs/decisions/README.md`](docs/decisions/README.md). Numeric `ADR-0001…` = core architecture;
 prefixed sets cover Color (`ADR-C*`), Efficiency (`ADR-E*`), Management (`ADR-M*`), Preview
 (`ADR-P*`), Resilience/AV (`ADR-R*`), Realtime (`ADR-RT*`), Timing (`ADR-T*`), Web (`ADR-W*`).
- **Research briefs:** [`docs/research/`](docs/research/) — see
 [`docs/research/README.md`](docs/research/README.md):
 - [Core Engine Architecture](docs/research/core-engine.md)
 - [Bulletproof Output, Resilience & A/V](docs/research/resilience-and-av.md)
 - [Efficiency on Commodity Hardware](docs/research/efficiency.md)
 - [Color Management](docs/research/color-management.md)
 - [Streaming Robustness Runbook](docs/research/streaming-gotchas.md)
 - [Preview Subsystem](docs/research/preview-subsystem.md)
 - [Realtime / Eventing API](docs/research/realtime-api.md)
 - [Management Capability Matrix](docs/research/management-capability-matrix.md)
 - [Web App + API Stack](docs/research/web-api-stack.md)
- **Reference:** [`docs/reference/`](docs/reference/) (bibliography, example streams).
- **Development:** [`docs/development/`](docs/development/) (e.g. completeness checklist).

---

## 10. Naming, style & house rules for agents

- **Crates** `mosaic-<area>` (kebab); **public types** `UpperCamel`; **functions/fields**
 `snake_case`; **features** `kebab-case`.
- **Errors:** per-crate `Error` enum via `thiserror`; application boundaries (e.g. `mosaic-cli`) may
 use `anyhow`.
- **Async** = `tokio`; **logging/tracing** = `tracing`; **serialization** = `serde`
 (adjacently-tagged enums `#[serde(tag="kind")]` for source/overlay/fit unions — **never**
 `untagged`).
- **Docs:** every public item documented; library crates carry `#![warn(missing_docs)]`.
- **Formatting/lint:** `rustfmt` (`rustfmt.toml`) and `clippy` clean (`-D warnings` in CI). Run both
 before proposing changes.

### Agent working agreement

- **Read [`docs/architecture/conventions.md`](docs/architecture/conventions.md) first.** If your
 change conflicts with it, you are wrong — or you must update conventions deliberately and call it
 out.
- **Do not invent** crate names, APIs, feature flags, or facts. Cross-check against conventions and
 the relevant ADR/brief; cite the ADR when justifying a design choice.
- **Keep the default build pure-Rust and CI-green without a GPU.** Gate all native/FFI/GPU code
 behind features; never make a hardware path mandatory for `cargo check`/`cargo test`.
- **Respect the invariants in §6** — especially output continuity, engine isolation, encode-once,
 and the color pipeline order. They are non-negotiable.
- **Prefer editing existing files** over creating new ones; do not add docs/READMEs unless asked.
- **Git etiquette:** branch off `main`; commit/push only when explicitly requested.
- **Safety:** prefer surgical, targeted commands. Never broadly kill processes by port; never
 disrupt shared infrastructure (containers, other sessions). When stopping a service, use its own
 stop mechanism. If in doubt, ask.
