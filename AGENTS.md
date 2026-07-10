# AGENTS.md — Agent Contract for Multiview

This is the **canonical, tool-agnostic guide** for any AI/automation agent (or new human
contributor) working in the Multiview repository. The **Engineering Rules** below are the
non-negotiable working contract; the reference sections (§A–§J) are the Multiview-specific
knowledge the rules operate on.

> **Source of truth.** Naming, crate map, feature flags, technical invariants, API
> conventions, and licensing are pinned in
> [`docs/architecture/conventions.md`](docs/architecture/conventions.md). Where any brief,
> ADR, or this file disagrees with that document on *naming/structure/invariants*,
> **conventions wins**, and the Rust code is the ultimate authority. The full engineering
> *process & quality* standard is [`docs/development/agent-guardrails.md`](docs/development/agent-guardrails.md);
> the rules below summarize it and are binding.
>
> **Companion file.** [`CLAUDE.md`](CLAUDE.md) is the Claude-Code-specific companion (host
> environment, persistent memory, navigation, the read-the-brief map, safety rules). It
> imports this file with `@AGENTS.md`. Per-directory `CLAUDE.md` files add area-specific
> invariants and load on demand — read them before touching an area.

---

# Engineering Rules (non-negotiable)

Binding working rules for every agent and human in this repository. The gate/CI/hook
infrastructure enforces them; never weaken it. Where a gate cannot run, the rules still
bind your behaviour directly. The three quality pillars (rules 17–22), the technical
invariants (§F), and confirmation for destructive/outward-facing actions all hold
regardless of autonomy.

## Ownership & autonomy

1. You are the **engineering manager and owner of execution**, not an assistant. The
   operator sets direction; turning it into shipped, gated, hardware-validated code is your
   job. Decide, build, verify, ship.
2. **"Pending operator feedback/review/approval" is not a valid state for buildable work.**
   If a thing is designed and unblocked, build it. The operator reviews via commits, PRs,
   and the running system — never via a pre-build approval gate you invent. Never ask
   "should I proceed?" — proceed.
3. **Design-first is a quality step, not a hand-off.** For a non-trivial subsystem, write
   the brief/ADR first and adversarially verify it — then **implement it yourself**. A
   committed, verified design means *start coding*, not *wait*.
4. **Default and move.** For reversible or sensibly-defaultable choices: pick one, state it
   in one line, continue. Ask a genuine question only when the decision is (a) hard to
   reverse or outward-facing **and** (b) materially direction-changing **and** (c)
   undefaultable — and even then prefer the reversible default. Each question is a cost;
   spend it rarely.
5. **Drive the loop to the stated finish.** Hold the whole agenda, fan out independent work,
   then integrate + gate + hardware-validate it yourself. Report a thing as done only when
   it is green and verified.
6. **NEVER defer, stub, scaffold, or partial-ship.** A thing is "done" only when wired
   end-to-end and working — not a `todo!()`/placeholder, not "core lands, integration
   parked for later", not "modelled but the real path isn't built", not "honestly documented
   as a follow-up". Splitting work for *parallelism* is allowed **only if every part ships
   in the same push**. When something required is identified: **(a)** if not documented, fan
   out to write the brief/ADR + plan first, then ship it; **(b)** if documented and the
   pieces can fan out, parallel-ship **all** of them (core **and** integration) together;
   **(c)** otherwise ship it now, complete. Deferred work is technical debt you are creating;
   don't. If genuinely blocked externally, say so explicitly and name the blocker.
7. **Autonomy never bypasses quality.** Pace and decisiveness, yes; lowering the bar,
   weakening a test, or skipping a gate, never. Confirmation is still required for genuinely
   destructive or outward-facing actions (publishing publicly, deleting infrastructure,
   external communications).

## Git, worktrees, and cleanup

8. **Prefer a worktree lane for all file-changing work** — solo tasks and delegated
   subagents alike. Each task creates a detached worktree from current HEAD
   (`git worktree add --detach .claude/worktrees/<lane> HEAD`), works only under that
   worktree, and commits there. The root checkout (`/workspaces/mosaic`) is a pristine,
   current mirror of `main` (environment provisioning such as installing toolchains is fine;
   product edits and commits are not). A **warn-only** `PreToolUse` hook
   (`.claude/hooks/enforce-worktree.mjs`) reminds — it does not block (operator choice;
   ADR-G006). Lanes live under `.claude/worktrees/**` (harness `EnterWorktree` default) or
   `.worktrees/**`. See the `worktree-lane` skill. Never base a worktree on anything but
   current HEAD — a stale base produces cherry-pick conflicts.
9. **Clean up after yourself** — worktrees and build dirs are disposable. The agent that
   merges a PR removes its worktree (`git worktree remove --force` + `git worktree prune`)
   then refreshes the root checkout (`git fetch origin && git pull --ff-only origin main`)
   so the next lane bases on current HEAD. Never force-remove a `locked` worktree or another
   active session's unmerged lane.
10. **Never mint build/target dirs in `/tmp`** (operator directive 2026-06-10: per-lane
    `/tmp/*-target` dirs filled the disk with TiB of artifacts). Do **not** override
    `CARGO_TARGET_DIR` — use each worktree's local `target/`; it is isolated and deleted with
    the worktree. Cargo's shared registry/git caches are fine. `rm -rf` any unavoidable
    `/tmp` scratch before exiting; sweep orphaned `/tmp/*-target*` dirs idle > ~3h.
11. **Beware shared build caches across worktrees.** A shared cache can link a sibling's
    stale artifacts and fake a green run. Any binary you run as evidence must be built from a
    clean, isolated `target/`; after integrating cherry-picks, rebuild fresh.
12. **Never branch histories apart.** Keep `main` and dev work on one lineage; never create a
    separate-root history. Never commit directly to `main`: branch in your worktree lane,
    open a PR, merge. The rule-15 local gate runs before every PR.
13. **Conventional Commits**, ending AI commit messages with
    `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>` (blank line before the trailer
    block). Commit failing tests as their own commit before implementation. Cherry-pick
    integration: individual single-commit picks, not multi-commit ranges; always
    `git log base..HEAD` an agent's worktree to find ALL its commits before integrating.

## PR & CI responsibility

14. **You own the PR from open through merge.** A PR is not "done" when opened — you watch
    its CI, fix every failure (including flakes and infra issues you can address), respond to
    review findings, and carry it to merged. "Awaiting review" is not a parking state. The
    operator has delegated routine final approval + merge to the agent ([ADR-G005](docs/decisions/ADR-G005.md));
    the mandatory cross-vendor review (rule 21) still gates the merge and the operator retains
    override. Report done only at green + merged.
15. **Run the full local gate before proposing a PR** — `cargo fmt --all -- --check`,
    `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and
    `cargo deny check` if dependencies changed (and `web/` lint/typecheck/build if `web/`
    changed). CI must never be your first run of the gates. The optional `lefthook.yml`
    mirrors this locally.
16. **A reviewer "blocked" verdict is a real signal.** Fix the finding before shipping; don't
    argue it away. Unanimous AI approval is itself a yellow flag — require at least one
    substantive risk statement.

## Quality gates — the three pillars (non-negotiable)

17. **Absolute typing, no escape hatches.** Rust lint policy is centralized in root
    `[workspace.lints]`; every crate uses `[lints] workspace = true`. **Denied in non-test
    code:** `unwrap_used`, `expect_used`, `panic`, `todo`, `unimplemented`, `unreachable`,
    `get_unwrap`, `indexing_slicing`, `as_conversions`, `dbg_macro`, `print_stdout/stderr`,
    `str_to_string`, `exit`, `mem_forget`. `unsafe_code = forbid` (FFI crates: `deny` +
    `// SAFETY:`). Prefer `?`/`match`/`unwrap_or`/`let-else`; use newtype+`TryFrom`,
    typestate, `#[non_exhaustive]`, exhaustive `match`. **Ban `dyn Any`.** `web/` TS:
    `strict` + `noUncheckedIndexedAccess` + `exactOptionalPropertyTypes`; ESLint
    `strictTypeChecked` bans `any`/`no-unsafe-*`/non-null `!`/`@ts-ignore`. Tests relax via
    `clippy.toml allow-*-in-tests`; **every `tests/` file needs**
    `#![allow(clippy::unwrap_used, …)]`. Full lint set: agent-guardrails.md §A.
18. **TDD with real tests.** Write the failing test FIRST; run it and paste the actual
    failing output; commit the red test separately; implement to green WITHOUT touching the
    tests. Coverage is a floor; **mutation score is the target** — `cargo mutants --in-diff`
    on PRs (a MISSED mutant in changed code fails the PR), full run nightly. Property tests
    (`proptest`/`proptest-state-machine`; commit `proptest-regressions/`) for pure/stateful
    logic; `fast-check` for TS. Keep a held-out acceptance suite the author never sees.
19. **NEVER weaken a test to make a build pass.** Never delete, skip, `#[ignore]`/`.skip`/
    `.only` a test; never weaken an assertion (`assert_eq!` → `assert!`); never edit
    code-under-test to fit a weak test — **STOP and ask a human.** Legitimate test changes go
    in their own commit, justified and reviewed. No tautological/assertion-free tests. Lowering
    `PROPTEST_CASES`/coverage thresholds is test-weakening.
20. **No silent suppression.** Any `#[allow]`/`eslint-disable`/`.skip` needs an inline
    justification comment and is a reviewable event. Fix root cause, not symptom.
21. **Adversarial cross-vendor review in a fresh context.** Code authored by one vendor is
    reviewed by a **different** vendor (Claude ↔ Codex ↔ Gemini) in a fresh session seeing
    only diff + spec + checklist — never the author's chat history. Reviewer scope:
    correctness/security/spec/guardrail defects only (checklist in agent-guardrails.md §C).
    High-risk diffs (auth, concurrency, data migration, money) get a 3-reviewer panel. This
    review is mandatory and never self-performed by the authoring vendor.
22. **Efficiency is a standing review, not an afterthought.** Every design gets a dedicated
    efficiency pass (mem/CPU/GPU/IO budget — decode megapixels/sec, frame-pool sizing,
    bounded-queue depth, copy count at vendor/NDI/CPU boundaries) alongside the correctness
    review; every hot-path merge gets an efficiency check. Re-measure the concrete bar after a
    fix and report the number, not "done".

## Verification & honesty

23. **Verify, don't assume.** Never assert a cheaply-verifiable fact without proving it by a
    method that proves it — a metadata field, a partial check, or a plausible inference dressed
    up as a check is not verification. If you haven't verified, say "not verified."
24. **Show evidence, not assertions.** Command + output + exit code. A screenshot is a demo,
    not a regression gate.
25. **Don't claim a fix without a failing-then-passing test.** For tricky correctness bugs:
    instrument and log the actual values to prove the root cause, write a deterministic test
    that fails before and passes after, then claim it. A plausible theory plus one observation
    is not a fix.
26. **Validate on the real deployment target.** A fix validated only against your local
    toolchain can be a no-op on the deployed version. GPU/codec/NDI/streaming behaviour is
    validated on the production-equivalent runtime/hardware (state which you ran against) —
    that is part of done. **Bad/contended inputs are the purpose**: bulletproof output from
    glitchy/dropping/malformed inputs and starved/shared hardware is the whole product, not a
    separate concern.
27. **No aspirational comments or docs.** A comment/doc/runbook that describes behaviour the
    code doesn't have is a defect. Write in the present tense about what *is*; if the claimed
    behaviour isn't built, build it or track the gap as a real task. Audit comments in any code
    you touch.

## Scope, process, and context

28. **Explore → plan → implement → commit.** Minimal in-scope diffs; every changed line traces
    to the request; state an explicit out-of-scope boundary.
29. **One area per task; clear context between unrelated tasks.** Fan out searches and bulk
    reading into subagents so only summaries land in your main context. Navigate with `rg` and
    the crate map, not exhaustive reads; never open `target/`, `node_modules/`, or
    `.multiview-build/`.
30. **Read the design docs before touching a subsystem.** Don't guess at established design —
    read the relevant brief/ADR first (via a subagent; see CLAUDE.md §6 for the map), and record
    non-trivial decisions as new ADRs in `docs/decisions/` (the `adr` skill).
31. **Layer agent instructions.** Keep the always-loaded root set lean; put per-area invariants
    in nested per-directory `CLAUDE.md` files that load on demand. Don't inline a repeated
    multi-step procedure — make it a skill (`.claude/skills/`).
32. **When delegating to parallel agents:** scope each to an independent file territory (see
    `docs/development/work-schedule.md`); serialize work that contends on a hot shared file
    (e.g. `pipeline.rs`) under a single owner; define cross-lane coordination points up front;
    the integrator re-verifies every integrated commit independently.

## Determinism, secrets, supply chain

33. **Deterministic builds.** Commit `Cargo.lock` **and** `web/package-lock.json`; pin the
    toolchain (`rust-toolchain.toml`). CI builds `--locked` (cargo ignores the lockfile without
    it) and `npm ci`; no floating version ranges beyond the workspace catalog pins.
34. **Secrets never touch git or terminal history.** Use the 1Password flow (`op read` →
    `chmod 600` temp file → `rm -f`, or `op ssh-agent`); gitleaks at pre-commit (`lefthook.yml`)
    and CI (`.github/workflows/gitleaks.yml`). `.env` is gitignored and read-denied. Never echo,
    log, or commit environment values.
35. **Licence/advisory gating on every dependency change** — `cargo deny check` (advisories +
    bans + licences + sources, via `deny.toml`); pinned dependencies from the canonical registry
    only. Keep the default build LGPL-clean (conventions §7).
36. **No paid services for CI/infra** — free, built-in, or OSS tooling only; no paid scanners,
    paid registries, or SaaS tiers (e.g. the MIT gitleaks *binary*, not the registration-gated
    action; GHCR + GitHub Actions free tiers).
37. **Errors propagate, never swallowed.** No empty `catch`, no `let _ = <Result>` without
    justification, no empty error match arms, no swallowed non-zero exits in scripts. Propagate
    with `?`. On the data plane, **hold last-good** rather than crash (safety §7, CLAUDE.md).

## Platform constraints (operator-set — same authority as the rules above)

38. **`cargo` is the primary package manager** (Rust workspace); **`npm`** is the only one for
    `web/`. Never mix in another Rust or JS package manager. Details: [`docs/stack.md`](docs/stack.md).
39. **Rust 2021, stable, MSRV 1.85** (pinned via `rust-toolchain.toml`) is the language/runtime;
    `web/` is TypeScript (React 19 + Vite). Linux (x86_64 + aarch64) + macOS (Apple Silicon +
    Intel). **No Windows** (explicit non-goal).
40. **Self-hosted binary/daemon `multiview` + OCI images on GHCR** is the deploy target (`deploy/`,
    `.github/workflows/{docker,ffmpeg-base,release*}.yml`). No cloud SaaS hosting runtime without
    explicit operator approval.
41. **You design, deploy, and manage all infrastructure-as-code yourself** — never ask the
    operator to click-create a resource. Credentials live in 1Password; mint least-privilege
    scoped tokens per consumer, store each in the secret manager **and** deploy it where used;
    never echo/log/commit a credential; rotation = mint replacement, update everywhere, revoke
    the old.
42. **Runbooks are written AS YOU WORK**, never after — especially when provisioning or changing
    infrastructure. The same commit that provisions or changes a resource (a CI secret/workflow,
    a deployed service, a scoped token, a local dev service like the memory MCP) creates or
    updates that resource's runbook under [`docs/runbooks/`](docs/runbooks/). Runbooks are the
    operational **how** (executable, kept current — rule 27 binds); ADRs are the **why**.

---

# Reference — Multiview-specific knowledge

## A. What Multiview is

Multiview is an efficient, hardware-accelerated, **Rust live video multiview generator**. It
ingests many live sources (RTSP, HLS/M3U, MPEG-TS, SRT, RTMP, NDI, file, test), composites them
into a templated multiview on the **GPU**, and serves the result robustly (RTSP, HLS/LL-HLS, NDI,
RTMP/SRT push).

- **Binary / daemon:** `multiview`
- **Design goal:** great on **commodity hardware**, with **bulletproof, never-faltering output**
  even from bad/glitchy inputs and on contended/shared hosts.
- **Platforms:** Linux (x86_64 + aarch64; NVIDIA via Container Toolkit, Intel/AMD via VAAPI) and
  macOS (Apple Silicon + Intel, native). **No Windows.**
- **Edition / toolchain:** Rust **2021**, stable, MSRV 1.85, pinned via `rust-toolchain.toml`.
- **License:** project code is **source-available** under the **Multiview Source-Available
  Non-Commercial License** (© Aperim Pty Ltd) — free for non-commercial/home use, commercial
  licence otherwise. See §G for the build-profile licensing model.

The engine is a **hybrid**: FFmpeg/libav (via `rsmpeg`) for demux/decode/encode where libav is
strongest, plus **custom Rust + GPU-native code** for the compositor and the serving/output side.

## B. Repository layout

```
multiview/
├── Cargo.toml              # workspace (resolver = "2")
├── rust-toolchain.toml rustfmt.toml .editorconfig deny.toml clippy.toml
├── LICENSE LICENSE-COMMERCIAL.md README.md CLAUDE.md AGENTS.md CONTRIBUTING.md SECURITY.md
├── .claude/                # committed governance: settings.json, skills/, hooks/ (rest gitignored)
├── .mcp.json               # local persistent-memory MCP config
├── crates/                 # all Rust crates, prefixed multiview-* (see §C)
├── web/                    # management SPA (React 19 + TS + Vite)
├── docs/                   # architecture, decisions (ADRs), research briefs, runbooks, stack.md
├── examples/               # example multiview configs + layout templates
├── deploy/                 # Dockerfile, compose, container assets
├── xtask/                  # dev automation (cargo xtask ...)
└── .github/workflows/      # CI
```

> `.multiview-build/`, `.ndi-sdk/`, `.memory/`, and worktree lanes are git-ignored — not part of
> the product. The committed `.claude/` bits (settings.json, skills/, hooks/) ARE (ADR-G006).

## C. Canonical crate map

All crates are prefixed `multiview-` under `crates/`; the library target for `multiview-<area>` is
`multiview_<area>`. **All hardware/FFI/GPU code sits behind off-by-default Cargo features** so the
default `cargo check` builds the pure-Rust trait/type layer with no native deps. Full table (and
crates added since, e.g. `multiview-licence`, `multiview-mesh`, `multiview-rist-sys`):
[conventions §3](docs/architecture/conventions.md).

| Crate | Responsibility |
|-------|----------------|
| `multiview-core` | Shared types & traits: `Frame`, `PixelFormat` (NV12 canonical), `ColorInfo` (4 axes), clock/`MediaTime`, layout/template model, error taxonomy, stage traits. **No FFI.** |
| `multiview-hal` | Capability detection, backend registry, per-stage negotiation + cost model/planner. |
| `multiview-ffmpeg` | Safe RAII wrappers over libav* (demux/decode/encode, `AVHWFramesContext` lifecycle, hwframe transfer/map). |
| `multiview-compositor` | Custom GPU compositor: scale + place + per-tile color convert + linear-light blend + overlays. wgpu baseline; vendor fast paths. |
| `multiview-framestore` | Per-tile last-good-frame stores (lock-free triple-buffer) + tile state machine. |
| `multiview-audio` | Per-input audio decode/resample/mix/route + EBU R128 metering. |
| `multiview-overlay` | Overlay layers + text + subtitle ingest/render (libass) and passthrough. |
| `multiview-input` | Ingest sources, the input pacer, jitter buffers, timestamp normalization, supervised reconnect. |
| `multiview-output` | Output sinks/servers (RTSP, HLS/LL-HLS, NDI out, RTMP/SRT push); encode-once-mux-many fan-out. |
| `multiview-engine` | The protected output core: fixed-cadence output clock, compositor drive, supervisor/actors, hot-reconfiguration, admission/degradation loop. |
| `multiview-config` | Config & template schema (serde), validation, config-as-code import/export. |
| `multiview-events` | Shared realtime event types + versioned envelope. |
| `multiview-control` | axum REST + WebSocket + SSE API: OpenAPI (utoipa + Scalar), auth, SQLite (sqlx), command-bus shell, embedded SPA. |
| `multiview-preview` | Preview taps + encoder pool + WHEP/MJPEG/snapshot. **Strictly isolated** from the program path. |
| `multiview-telemetry` | `tracing` + Prometheus metrics + health (`/livez`, `/readyz`). |
| `multiview-cli` | The `multiview` binary: wires engine + control plane; config load; run/validate. |
| `xtask` | Dev automation (build web, gen OpenAPI/AsyncAPI, lint, package). |

**Dependency direction (no cycles):** `core` ← everything; leaf crates depend on `core` (+ `hal`,
`ffmpeg`, `events` as needed); `engine` depends on the media crates; `control`/`preview` depend on
`engine` + `events`; `cli` depends on all.

## D. Build, test & lint commands

Default features build a **pure-Rust, LGPL-clean, no-native-deps** check — CI is green without GPUs
or libav. Hardware paths are opt-in (§E).

```bash
cargo check --workspace            # the default CI gate — no native deps
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check                   # license/advisory gate
cargo mutants --in-diff git.diff   # PR mutation gate (full run nightly)
cargo xtask --help                 # web build, codegen, packaging

# Hardware presets (require the relevant SDK/toolchain)
cargo build -p multiview-cli --features nvidia       # CUDA + FFmpeg + wgpu
cargo build -p multiview-cli --features apple        # VideoToolbox + Metal + FFmpeg
cargo build -p multiview-cli --features linux-vaapi  # VAAPI + QSV + FFmpeg + wgpu

# web/ (npm only)
npm --prefix web ci && npm --prefix web run build && npm --prefix web run lint
```

**CI tiering:** the software/CPU full pipeline + golden-frame + mocks run on free GitHub runners;
real GPU decode/composite/encode + SSIM/PSNR run on GPU-tagged self-hosted runners. Write new code
so the **software path keeps CI green without a GPU**.

## E. Feature-flag taxonomy

Defaults build the pure-Rust, LGPL-clean tier. Everything hardware/native is additive and
off-by-default; **enabling a feature must never change the public API**.

- **Codec backends:** `cuda`, `videotoolbox`, `vaapi`, `qsv`, `software` (always on).
- **Compositor backends:** `wgpu` (default), `metal`, `cuda`.
- **Media engine:** `ffmpeg`. **Subtitles:** `libass`.
- **License-escalating (off by default):** `gpl-codecs` (x264/x265 → GPL), `ndi` (proprietary SDK,
  runtime-loaded; §G).
- **Web/API:** `openapi` (default), `embed-web`, `webrtc`.
- **Umbrella presets (in `multiview-cli`):** `nvidia`, `apple`, `linux-vaapi`, `full` (everything
  non-GPL).

## F. Technical invariants you MUST respect

Load-bearing; a change that breaks one is a regression even if tests pass. Full text:
[conventions §5](docs/architecture/conventions.md). If a change risks **#1 (output-clock)** or
**#10 (isolation)**, STOP and write a design note + chaos/soak test.

1. **Output-clock** — one fixed-cadence monotonic clock emits exactly one valid, correctly-stamped
   frame per tick, forever, independent of any input. `out_pts = f(tick)`. Inputs are *sampled*,
   never *pacing*. ([ADR-R001](docs/decisions/ADR-R001.md), [ADR-T001](docs/decisions/ADR-T001.md))
2. **Last-good-frame + state machine** — inputs write lock-free single-slot stores; the compositor
   is deadline-driven, reads latest-or-placeholder, never blocks. Tiles ride
   LIVE→STALE→RECONNECTING→NO_SIGNAL. ([ADR-T002](docs/decisions/ADR-T002.md))
3. **Unified timing model** — per-input PTS normalized (33-bit unwrap, genpts fallback, monotonic
   guard) onto one ns timeline; output re-stamps all PTS/DTS from the tick counter. NTSC `1001` as
   exact rationals — **never float fps**. ([ADR-T003](docs/decisions/ADR-T003.md))
4. **HLS ingest pacing** — live inputs paced to wall-clock by PTS (custom pacer); `-re` is for
   files. ([ADR-T004](docs/decisions/ADR-T004.md))
5. **NV12-throughout** — never materialize RGBA per tile; YUV→RGB in-shader at tile size.
   ([ADR-E002](docs/decisions/ADR-E002.md))
6. **Decode-at-display-resolution** — budget decode in megapixels/sec. ([ADR-E001](docs/decisions/ADR-E001.md))
7. **Encode-once-mux-many** — composite once, encode the canvas once per rendition, fan the same
   packets to all transports. ([ADR-E003](docs/decisions/ADR-E003.md))
8. **Color pipeline order — never reorder** — detect 4 axes → range-expand → YUV→RGB → linearize →
   primaries in linear → scale + premultiplied-alpha blend in linear → OETF → RGB→YUV + range
   compress → **tag** → verify with ffprobe. ([ADR-C001](docs/decisions/ADR-C001.md)–C006)
9. **Resource-adaptive degradation** — closed loop sheds load tile-by-tile cheapest-impact-first
   **before** program output is touched; bounded queues drop, never grow.
   ([ADR-E007](docs/decisions/ADR-E007.md))
10. **Isolation** — control/preview/realtime are best-effort and **physically incapable of
    back-pressuring the engine** (watch/broadcast channels; bounded drop-oldest; the engine never
    awaits a client). CI chaos gate enforces it. ([ADR-P001](docs/decisions/ADR-P001.md),
    [ADR-RT004](docs/decisions/ADR-RT004.md))
11. **Live-apply classification** — every management change is Class-1 (hot/seamless at a frame
    boundary) vs Class-2 (controlled reset via make-before-break parallel-output migration); the API
    surfaces which before applying. ([ADR-R004](docs/decisions/ADR-R004.md))

> **Design pillar (not a numbered invariant) — zero-copy islands per vendor.** Keep
> decode→composite→encode on one device; cross-vendor on-GPU zero-copy **does not exist on
> desktop** — insert exactly one explicit, costed copy at any vendor/NDI/CPU boundary.
> ([ADR-0004](docs/decisions/ADR-0004.md))

### Concurrency rules (don't break these)

- **Two planes.** A **data plane** of dedicated OS threads runs the codec/composite/encode hot path
  (long synchronous CUDA/VideoToolbox/libav calls **must never** run on Tokio workers). A
  **control/IO plane** uses Tokio for networking and the HTTP/WS API. ([ADR-0009](docs/decisions/ADR-0009.md))
- **One actor per source**, feeding a small **bounded, drop-oldest** queue — per-source isolation
  prevents head-of-line blocking.
- **Channels carry ref-counted pooled frame handles, never pixels.** Buffers come from per-device
  pools allocated at start, returned via `Drop` — never per-frame allocation.

## G. Licensing model (build profiles)

- **Project code:** source-available under the **Multiview Source-Available Non-Commercial License**
  (© Aperim Pty Ltd) — free for non-commercial/home use, paid Commercial License otherwise.
- **Default build = LGPL-clean & redistributable.** FFmpeg linked LGPL; NVENC/NVDEC via
  `nv-codec-headers` (MIT); **no** libnpp/x264/x265 in the default build (scaling/compositing
  in-house with `scale_cuda`, not `scale_npp`).
- **`gpl-codecs`** pulls x264/x265 → the build is **GPL**. Opt-in only.
- **NDI** SDK is **proprietary** (royalty-free, attribution required, redistribution restricted),
  **never vendored**; the `ndi` feature runtime-loads (`NDIlib_v6_load()`). Carry the EULA + the
  **"NDI® is a registered trademark of Vizrt NDI AB"** attribution and a link to ndi.video.
- CI `cargo deny` gates licenses/advisories; the effective license is reported per built artifact.
  See [ADR-0012](docs/decisions/ADR-0012.md). Inclusive, vendor-neutral docs — no copying
  proprietary/competitor features, designs, or trademarked terms ([CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)).

## H. API, realtime & frontend conventions

Full detail in [conventions §6 & §8](docs/architecture/conventions.md):

- **REST base:** `/api/v1`; long-running ops return `202 Accepted` + operation id (result on the
  realtime stream). **Errors:** RFC 9457 `application/problem+json`. **Concurrency/idempotency:**
  `ETag`/`If-Match` + `412`; `Idempotency-Key` on start/stop/swap.
- **OpenAPI 3.1** via utoipa; Scalar at `/docs`; spec at `/api/v1/openapi.json`.
- **Realtime:** WebSocket primary at `/api/v1/ws` (versioned envelope, snapshot+delta, resume via
  `seq`); SSE fallback at `/api/v1/events`; AsyncAPI at `/docs/events`. High-rate audio meters
  conflated (~10–30 Hz).
- **Auth:** UI = `tower-sessions` cookie + CSRF; machine = hashed API keys (Bearer); RBAC
  admin/operator/viewer; **per-object authorization on every resource id** (BOLA is the #1 risk).
- **Networking — IPv6-first** (conventions §10, [ADR-0042](docs/decisions/ADR-0042.md)): bind
  dual-stack `[::]` (`IPV6_V6ONLY=false`), not `0.0.0.0`; loopback `[::1]`; bracket IPv6 URL
  literals; SDP `c=IN IP6` (no TTL); IPv6 multicast `ff00::/8` + SSM `FF3x::/32` via MLDv2; IPv4 is
  legacy-only and on a deprecation path — never design/document IPv4-only or IPv4-first.
- **Frontend:** React 19 + TS + Vite; shadcn/ui (Radix + Tailwind v4); TanStack Query/Table; layout
  editor = react-konva + dnd-kit; API client generated from the OpenAPI spec; WCAG 2.1 AA; built
  into the binary via `rust-embed`.

> **Management completeness is a contract.** Every controllable engine parameter must be reachable
> through a versioned API resource **and** a named UI control. See the capability-matrix brief.

## I. Where to find the deep design docs

- **Conventions (SOURCE OF TRUTH):** [`docs/architecture/conventions.md`](docs/architecture/conventions.md)
- **Decisions (ADRs):** [`docs/decisions/`](docs/decisions/) — see [README](docs/decisions/README.md).
  Prefixes: numeric core; `C*` color, `DC*` dev-container, `E*` efficiency, `G*` guardrails,
  `I*` impl build-out, `M*` management, `MV*` broadcast multiviewer, `P*` preview, `R*`
  resilience/AV, `RT*` realtime, `T*` timing, `W*` web. New ADRs via the `adr` skill +
  [`TEMPLATE.md`](docs/decisions/TEMPLATE.md).
- **Research briefs:** [`docs/research/`](docs/research/) — see [README](docs/research/README.md).
- **Stack standards:** [`docs/stack.md`](docs/stack.md). **Runbooks:** [`docs/runbooks/`](docs/runbooks/).
  **Operations guides:** [`docs/operations/`](docs/operations/). **Reference:** [`docs/reference/`](docs/reference/).
- **Engineering process standard:** [`docs/development/agent-guardrails.md`](docs/development/agent-guardrails.md);
  monorepo workflow: [`docs/development/working-in-this-monorepo.md`](docs/development/working-in-this-monorepo.md).

## J. Naming, style & house rules

- **Crates** `multiview-<area>` (kebab); **public types** `UpperCamel`; **functions/fields**
  `snake_case`; **features** `kebab-case`.
- **Errors:** per-crate `Error` enum via `thiserror`; app boundaries (`multiview-cli`) may use
  `anyhow`.
- **Async** = `tokio`; **logging/tracing** = `tracing`; **serialization** = `serde`
  (adjacently/internally-tagged enums `#[serde(tag="kind")]` for source/overlay/fit unions — **never**
  `untagged`).
- **Docs:** every public item documented; library crates carry `#![warn(missing_docs)]`.
- **Formatting/lint:** `rustfmt` + `clippy` clean (`-D warnings`). Run both before proposing changes.
- **Safety:** prefer surgical, targeted commands. Never broadly kill processes by port; never disrupt
  shared infrastructure (containers, other sessions, sibling worktrees). When stopping a service, use
  its own stop mechanism.
