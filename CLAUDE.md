@AGENTS.md

# CLAUDE.md — Agent Guide for the Multiview Repository

This is the Claude-Code-specific companion to [`AGENTS.md`](AGENTS.md). **`AGENTS.md`
governs *how you work*** (the 42 Engineering Rules + the Multiview reference sections) and
is imported above. **This file orients you in the repo**: the persistent memory, how to
navigate the monorepo without burning context, which deep brief to read before touching a
subsystem, and the **safety rules you must never break**.

> **Source of truth:** [`docs/architecture/conventions.md`](docs/architecture/conventions.md)
> pins the canonical crate names, API paths, feature flags, invariants, and licensing. Where
> any other doc disagrees, **conventions win**, and the Rust code is the ultimate source of
> truth. The technical invariants and crate map live in `AGENTS.md` §F/§C; this file does not
> repeat them — it points to them.

---

## Persistent memory (MCP)

A fully-local vector-RAG memory server (`memory`, `mcp-server-qdrant`) is configured in
[`.mcp.json`](.mcp.json); data lives under `.memory/` (gitignored). It is the
**repository-scoped, committed-config, team-shared** memory — decisions, operator feedback,
gotchas, milestone state.

- Run **`qdrant-find`** at the start of any non-trivial task to recall prior decisions, before
  re-deriving anything a past session may have settled.
- **`qdrant-store`** non-obvious decisions, operator corrections, and hard-won gotchas when you
  learn them — proactively, not on request.
- Conventions: the [`memory` skill](.claude/skills/memory/SKILL.md). Runbook:
  [`docs/runbooks/memory-mcp.md`](docs/runbooks/memory-mcp.md). The store is single-process —
  one session per repo clone at a time.

> Claude Code also keeps its own per-user file-based memory (under `~/.claude/…`) that it
> manages automatically. That is complementary and personal; the MCP above is the shared,
> committed project memory.

## Skills & hooks

On-demand skills under [`.claude/skills/`](.claude/skills/) — invoke the relevant one:

- **`worktree-lane`** — create/work-in/integrate/clean-up an isolated worktree lane (rules 8–13).
- **`adr`** — record a decision in `docs/decisions/` (rule 30).
- **`memory`** — `qdrant-find`/`qdrant-store` conventions.

A **warn-only** `PreToolUse` hook ([`.claude/hooks/enforce-worktree.mjs`](.claude/hooks/enforce-worktree.mjs))
reminds when an edit targets the root checkout instead of a lane (it does not block; ADR-G006).
Don't inline a recurring multi-step procedure into this file — make it a skill (rule 31).

---

## Ownership & quality (see AGENTS.md)

The full working contract is `AGENTS.md` rules 1–42. The load-bearing reminders:

- **You run this** — decide, build, verify, ship; never park buildable work "pending approval"
  (rules 1–2). **Never defer/stub/scaffold/partial-ship** (rule 6).
- **The three pillars are blocking** — absolute typing, TDD-first with real tests, adversarial
  cross-vendor review (rules 17–22; full standard:
  [`docs/development/agent-guardrails.md`](docs/development/agent-guardrails.md)).
- **Verify, don't assume; show evidence; validate on real hardware** (rules 23–26).
- **Bad/contended inputs are the purpose** — bulletproof output from glitchy/dropping inputs and
  starved/shared hosts is the whole product (rule 26 + safety §7).

---

## Working in this monorepo — navigation & context discipline

Multiview is a complex monorepo (a large Cargo workspace + `web/` SPA + a large `docs/` tree of
research briefs and 169 ADRs). To work efficiently here without exhausting your context window,
follow the official Claude Code guidance for large/complex codebases:
[code.claude.com/docs/en/large-codebases](https://code.claude.com/docs/en/large-codebases). The
full agent playbook is [`docs/development/working-in-this-monorepo.md`](docs/development/working-in-this-monorepo.md);
the one-screen layout is [`docs/development/codebase-map.md`](docs/development/codebase-map.md).

**Layered, on-demand instructions.** Each crate has a short `crates/<crate>/CLAUDE.md` (and the
SPA has `web/CLAUDE.md`) with that area's invariants and the exact brief(s)+ADRs to read first.
A subdirectory `CLAUDE.md` **loads automatically when Claude reads a file in that directory** —
so you pay context only for the crate you're in. **Start Claude from the crate you're working
in** to load just the root files plus that crate's file. (This root `CLAUDE.md` + `AGENTS.md`
are re-injected after `/compact`; nested crate files reload on the next read in that crate.)

**Context discipline.**
- Work **one crate/area per task**; `/clear` between unrelated tasks.
- **Fan out searches into subagents** — when a side task means reading many files (find a
  symbol's callers, summarize a brief, audit a diff against invariants), delegate it so the file
  reads stay out of your main thread and you get back only the summary.
- Navigate with `rg` and the crate map, not exhaustive reads. Never open `target/`,
  `node_modules/`, or `.multiview-build/` (git-ignored, excluded from search, and read-denied).
- **All file-changing work goes in a worktree lane** (`worktree-lane` skill), not the root
  checkout (rule 8; warn-only hook).

**The core workflow: read the brief before touching subsystem X.** The per-crate `CLAUDE.md`
files and §6 below name the brief and ADRs for each subsystem — read them (via a subagent)
before writing code. Always re-check invariants **#1 (output-clock)** and **#10 (isolation)**;
a change that risks either means stop and write a design note.

---

## 3. Crate map — where things live

Canonical responsibilities and dependency direction: `AGENTS.md` §C / [`conventions §3`](docs/architecture/conventions.md).
This table is the *navigation* lens — "you touch it when…" + the optional features per crate.

| Crate | You touch it when… | Optional features |
|-------|--------------------|-------------------|
| `multiview-core` | Editing shared types/traits: `Frame`, `PixelFormat` (NV12 canonical), `ColorInfo` (4 axes), clock/`MediaTime`, layout/template model, error taxonomy, stage traits (`Source`, `Sink`, `Decoder`, `Encoder`, `Compositor`, `Backend`). **No FFI.** | — |
| `multiview-hal` | Capability detection, backend registry, per-stage negotiation + cost model/planner (admission/degradation inputs). | `cuda`, `vaapi`, `qsv`, `videotoolbox` |
| `multiview-ffmpeg` | Safe RAII wrappers over libav\* (demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe transfer/map). All raw FFI is owned here. | `ffmpeg`, `gpl-codecs` |
| `multiview-compositor` | The custom GPU compositor: scale + place + per-tile color convert + linear-light blend + overlay compositing. wgpu baseline; vendor fast paths. | `wgpu` (default), `cuda`, `metal`, `vaapi` |
| `multiview-framestore` | Per-tile last-good-frame stores (lock-free triple-buffer) + the tile state machine. | — |
| `multiview-audio` | Per-input audio decode/resample/mix/route (program bus + discrete tracks) + EBU R128 metering. | `ffmpeg` |
| `multiview-overlay` | Overlay layers + text rendering + subtitle ingest/render (libass) and passthrough. | `libass` |
| `multiview-input` | Ingest sources, the **input pacer**, jitter buffers, timestamp normalization, supervised reconnect. | `ffmpeg`, `ndi` |
| `multiview-output` | Output sinks/servers: RTSP server, HLS/LL-HLS packager, NDI out, RTMP/SRT push; encode-once-mux-many fan-out. | `ffmpeg`, `ndi` |
| `multiview-engine` | The **protected output core**: fixed-cadence output clock, compositor drive, supervisor/actors, hot-reconfiguration, admission/degradation loop. | — |
| `multiview-config` | Config & template schema (serde), validation, config-as-code import/export. | — |
| `multiview-events` | Shared realtime event types + versioned envelope. | — |
| `multiview-control` | axum REST + WebSocket + SSE API: OpenAPI (utoipa+Scalar), auth, SQLite (sqlx), command-bus shell, embedded SPA. | `openapi` (default), `embed-web` |
| `multiview-preview` | Preview taps (input/program/output), preview encoder pool, WHEP/MJPEG/snapshot. **Strictly isolated** from the program path. | `webrtc` |
| `multiview-telemetry` | `tracing` + Prometheus metrics + health (`/livez`, `/readyz`). | — |
| `multiview-cli` | Binary **`multiview`**: wires engine + control plane; config load; run/validate. | aggregates feature flags |
| `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint, package). | — |

Additional crates exist beyond this core set (e.g. `multiview-licence`, `multiview-mesh`,
`multiview-rist-sys`) — check `crates/` and their `CLAUDE.md`. Some deep briefs use older crate
names (`multiview-sys`, `multiview-io`, `multiview-server`) — **use the canonical names**, not the
brief's working names. `.multiview-build/` is a git-ignored transient dir — never commit from it.

## 4. Build / test / lint — quick reference

Full command set + CI tiering: `AGENTS.md` §D. Day-to-day:

```bash
cargo check --workspace                 # CI-green baseline (no native deps, GPU-free)
cargo test --workspace                  # software/CPU backends run everywhere
cargo test -p multiview-<area>          # a single crate
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check                        # licenses + advisories
cargo xtask build-web                   # build the SPA + stage for rust-embed
cargo xtask gen-openapi                 # regenerate OpenAPI/AsyncAPI specs
```

- **Default `cargo check` must stay green without GPUs.** Never make a hardware feature
  default-on. GPU decode/composite/encode + SSIM/PSNR run only on GPU-tagged self-hosted runners
  (golden-frame on CPU only; GPU output uses SSIM/PSNR thresholds, never bit-exact).
- Build artifacts stay worktree-local — **never set `CARGO_TARGET_DIR` to `/tmp`** (rule 10).

## 5. Coding conventions

Essentials are in `AGENTS.md` §J/§H + [`conventions §6,§8,§9,§10`](docs/architecture/conventions.md):
naming, per-crate `thiserror` errors, `tokio`/`tracing`/`serde` (adjacently-tagged enums, never
`untagged`), IPv6-first networking, `#![warn(missing_docs)]`, rustfmt/clippy-clean, the REST/WS API
conventions, and the React 19 + Vite frontend stack.

---

## 6. Read these briefs BEFORE touching subsystem X

Don't guess at a subsystem's design — the briefs are verification-hardened and capture the footguns.
Read the relevant one (and its ADRs) **before** writing code:

| Working on… | Read first |
|-------------|------------|
| Output clock, compositor drive, supervisor, hot-reconfig (`multiview-engine`) | [core-engine](docs/research/core-engine.md) §4–§12, [resilience-and-av](docs/research/resilience-and-av.md), [streaming-gotchas §0](docs/research/streaming-gotchas.md), ADR-T001/R001/R004 |
| HAL, capability detection, backend negotiation/planner (`multiview-hal`) | [core-engine §6](docs/research/core-engine.md), [efficiency](docs/research/efficiency.md), ADR-0003/0004/E008 |
| libav wrappers, hwaccel, frame pools, FFI (`multiview-ffmpeg`) | [core-engine §7,§8.1,§12](docs/research/core-engine.md), ADR-0002/0004 |
| GPU compositor, color math, fit/crop/overlays (`multiview-compositor`) | [color-management](docs/research/color-management.md), [core-engine §8.2,§13](docs/research/core-engine.md), ADR-C001..C006, ADR-E002 |
| Frame stores + tile state machine (`multiview-framestore`) | [resilience-and-av](docs/research/resilience-and-av.md), [streaming-gotchas §1,§7](docs/research/streaming-gotchas.md), ADR-T002 |
| Ingest, input pacer, jitter, PTS normalization, reconnect (`multiview-input`) | [streaming-gotchas §1–§3,§5–§7](docs/research/streaming-gotchas.md), [core-engine §9.1](docs/research/core-engine.md), ADR-T003/T004/T006/T007/T008 |
| Output servers, RTSP/HLS·LL-HLS/NDI/push (`multiview-output`) | [streaming-gotchas §4](docs/research/streaming-gotchas.md), [core-engine §9.2](docs/research/core-engine.md), ADR-0006/0007/T005 |
| Audio decode/mix/route + R128 (`multiview-audio`) | [resilience-and-av](docs/research/resilience-and-av.md), [streaming-gotchas §5,§7](docs/research/streaming-gotchas.md), ADR-R005/R006/T006 |
| Overlays + subtitles (`multiview-overlay`) | [resilience-and-av](docs/research/resilience-and-av.md), ADR-R007/R008 |
| Config/template schema, validation, config-as-code (`multiview-config`) | [core-engine §13,§14](docs/research/core-engine.md), [management-capability-matrix](docs/research/management-capability-matrix.md), ADR-0010 |
| Control API: REST/WS/SSE, auth, command bus (`multiview-control`) | [web-api-stack](docs/research/web-api-stack.md), [realtime-api](docs/research/realtime-api.md), [management-capability-matrix](docs/research/management-capability-matrix.md), ADR-RT001..RT006, ADR-W001..W008 |
| Events/envelope (`multiview-events`) | [realtime-api](docs/research/realtime-api.md), ADR-RT002/RT003 |
| Preview taps, WHEP/MJPEG, cue/pre-warm (`multiview-preview`) | [preview-subsystem](docs/research/preview-subsystem.md), ADR-P001..P005 |
| Telemetry, metrics, health (`multiview-telemetry`) | [core-engine §15](docs/research/core-engine.md), [resilience-and-av](docs/research/resilience-and-av.md), ADR-R009 |
| Any management surface (API ↔ UI ↔ engine) | [management-capability-matrix](docs/research/management-capability-matrix.md) — the authoritative capability table |
| Licensing / build profiles | [core-engine §17,§18](docs/research/core-engine.md), [`conventions.md` §7](docs/architecture/conventions.md), ADR-0011/0012 |

Indexes: all briefs in [`docs/research/`](docs/research/README.md); all decisions in
[`docs/decisions/`](docs/decisions/README.md) (169 ADRs, grouped by area).

---

## 7. Safety rules (engine + FFI + process)

These are non-negotiable for code in this repo (they make invariants #1 and #10 concrete).

1. **Never break the output-clock invariant.** The output stage emits one valid, correctly-timestamped
   frame per tick forever. No code path on the data plane may block waiting for an input, a client, or a
   lock that an input/client holds. Inputs are sampled; they never pace.
2. **Preview / control / realtime must never back-pressure the engine.** Use watch/broadcast channels
   and bounded **drop-oldest** queues. The engine **never `.await`s a client** and never sends on a
   channel that a slow consumer can fill. Conflate high-rate telemetry (audio meters ~10–30 Hz). A CI
   chaos gate enforces this — if you add a channel from engine→outside, prove it can't stall the engine.
3. **No `unwrap`/`expect`/`panic!` on the hot path** (decode→composite→encode→mux, the output clock,
   frame stores). Hot-path code returns/handles errors and **holds last-good** rather than crashing.
   `unwrap` is acceptable only in tests and in clearly non-hot startup/config code with a justification.
4. **FFI safety:** all raw libav/CUDA/Metal/Vulkan/NDI FFI lives behind safe wrappers (FFI is owned by
   `multiview-ffmpeg` and the feature-gated backend modules). `unsafe` blocks carry a `// SAFETY:` comment
   stating the invariant upheld. libav context wrappers are `Send + !Sync` (or Mutex-guarded); never
   share one context across threads unsynchronized. `get_format` and other `extern "C"` callbacks run on
   foreign/decoder threads — keep them allocation-light and never let a Rust panic unwind across the FFI
   boundary. Return buffers to pools via `Drop`; never run that `Drop` inside a Tokio async destructor.
5. **Bounded memory everywhere on the data plane.** Queues drop, never grow. No unbounded channels into
   the engine. Frame buffers come from per-device pools allocated at start, never per-frame.
6. **Timestamps:** never feed raw input PTS to the encoder/muxer — re-stamp from the tick counter. Carry
   internal time as i64 ns / exact rationals; **never float fps** (drifts ~3.6 s/hour).
7. **Color order is fixed** (invariant #8). Range is handled in-shader exactly once. Tagging ≠
   converting — always tag the output and verify with ffprobe.
8. **Licensing discipline:** keep the default build LGPL-clean. Do scaling/compositing in-house
   (`scale_cuda`, not `scale_npp`). `gpl-codecs` (x264/x265) makes the whole build GPL — opt-in only.
   Never vendor the proprietary NDI SDK; the `ndi` feature is runtime-loaded (`NDIlib_v6_load()`) with
   mandatory attribution. CI `cargo deny` gates this. See [`conventions.md` §7](docs/architecture/conventions.md).

---

## 8. Navigating `docs/`

```
docs/
├── architecture/conventions.md  # SOURCE OF TRUTH — read this first, always
├── research/                    # deep, verification-hardened design briefs (the "why")
├── decisions/                   # ADRs (the load-bearing decisions; 169, grouped by area) + TEMPLATE.md
├── runbooks/                    # operational "how" for provisioned resources (rule 42)
├── operations/                  # broader operational guides (building, container, devcontainer, …)
├── stack.md                     # toolchain & platform standards (rules 38–42)
├── reference/                   # bibliography, example streams
└── development/                 # agent-guardrails, work-schedule, completeness checklist, process docs
```

- Start at [`conventions.md`](docs/architecture/conventions.md) for canonical names/paths/flags.
- For *why* a subsystem is built a certain way, read its brief in
  [`docs/research/`](docs/research/README.md) (see §6 above for the mapping).
- For a specific decision + alternatives/consequences, find the ADR in
  [`docs/decisions/`](docs/decisions/README.md). Prefixes track area: numeric core; `R*`
  resilience/AV, `E*` efficiency, `C*` color, `T*` streaming/timing, `P*` preview, `RT*` realtime,
  `M*` management, `MV*` broadcast multiviewer, `G*` guardrails/governance, `DC*` dev-container,
  `I*` impl build-out, `W*` web/API.
- Prefer linking to a brief over duplicating it. When code and a brief disagree, the code wins and
  the brief should be updated — flag the drift.

---

## 9. Git & workflow expectations

The full workflow is `AGENTS.md` rules 8–16 (worktree lanes, cleanup, Conventional Commits,
PR-from-open-through-merge ownership). For Claude specifically:

- **Work in a worktree lane, never the root checkout** (`worktree-lane` skill; warn-only hook).
  Branch in the lane, open a PR, and **own it to green CI + merge** — the agent owns routine
  review→merge (ADR-G005), the operator retains override. "Commit/push only when asked" is no
  longer the rule: the operator sets direction, you drive the PR lifecycle (rule 14).
- End commit messages with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`; end PR
  bodies with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Run the full local gate before every PR (rule 15): `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and
  `cargo deny check` if dependencies changed.
- Don't add Windows support, per-tile re-encode/ABR-per-tile, or cross-vendor on-GPU zero-copy —
  explicit **non-goals** ([core-engine §2](docs/research/core-engine.md)).
