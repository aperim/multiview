# Runbook — the delivery loop (scheduled ticks)

The backlog is driven by **one cycle per Claude Code session**, started by a scheduler.
The generic cycle is the [`orchestrate` skill](../../.claude/skills/orchestrate/SKILL.md);
this runbook is Multiview's local half — the lane map, the gates a cycle owes here, and
the operator procedures. The decision is [ADR-G009](../decisions/ADR-G009.md); the
single-integrator/territory model it builds on is [ADR-G007](../decisions/ADR-G007.md).

**Why ticks and not a self-continuing session.** A session that re-enters its own loop
re-sends its whole transcript every turn. Measured across 1,589 sessions, cache reads are
59.5% of true cost and the 4% of sessions past 250 turns are 66% of spend — $0.47/session
at 16–40 turns against $40.70 above 600. Continuity therefore lives in GitHub, not in a
context window. Nothing here may call `ScheduleWakeup`, block a `Stop` hook, or loop back
in-context.

## Current state

The machinery is installed and **the loop is disarmed**. It stays disarmed until the
prerequisites below are met — arming it now would dispatch against an empty label set.

| Thing | State |
| --- | --- |
| `.claude/skills/orchestrate/` (skill, `tick.sh`, `lib/`) | installed; `node .claude/skills/orchestrate/lib/test.mjs` → 22/22 |
| `orchestrate.config.json` | written; lanes derived from the territory map below |
| `.claude/loop/ACTIVE` | **absent** — not armed |
| `status: ready` / `status: blocked` / `loop-state` labels | **do not exist yet** |
| Backlog substrate | `docs/development/work-schedule.md` (402 KB board), **not** GitHub issues |

### Prerequisites before arming

1. **Create the labels.** The cycle's Orient step filters on them; without them
   `gh issue list --label "status: ready"` returns nothing and every cycle is a no-op.

   ```sh
   gh label create "status: ready"   --description "Dependency-ready; an orchestrator cycle may dispatch it" --color 0E8A16
   gh label create "status: blocked" --description "Blocked on a human or an unmerged dependency"            --color B60205
   gh label create "loop-state"      --description "The orchestrator's cross-cycle state issue"              --color 5319E7
   ```

2. **Open the loop-state issue** (one, labelled `loop-state`) holding `ownerRunId`,
   `heartbeatAt`, `consecutiveDryCycles` and the reservation ledger. The cycle reads it
   **by number**; record that number here once it exists.

3. **Decide the backlog substrate.** The cycle reads GitHub issues. Multiview's backlog is
   still the markdown board. Either promote ready items to issues carrying a `lane:` marker,
   or point `readyLabel` at whatever convention replaces it. Until then a cycle has nothing
   to dispatch. **This is an open operator decision, not a defect in the machinery.**

## Run a cycle

```sh
mkdir -p .claude/loop          # once; .claude/loop/ is gitignored runtime state
touch .claude/loop/ACTIVE      # arm
./.claude/skills/orchestrate/tick.sh
```

`tick.sh` takes a lock, runs exactly one cycle in a **fresh** `claude` process, and exits.
It is the scheduler's entry point and never self-schedules.

| Sentinel / var | Effect |
| --- | --- |
| `.claude/loop/ACTIVE` | armed; absent = every tick is a no-op |
| `.claude/loop/STOP` | stop after the current cycle |
| `.claude/loop/DONE` | the DONE gate fired; `rm` it to resume |
| `.claude/loop/tick.lock` | single-owner lock; a dead pid's lock is reclaimed automatically |
| `ORCH_MAX_SECONDS` | per-cycle ceiling (default 3600); on timeout the next tick resumes from GitHub |

Cadence — pick one, never something that keeps a session alive between cycles:

```sh
# cron, every 15 minutes
*/15 * * * * cd /path/to/multiview && ./.claude/skills/orchestrate/tick.sh >>.claude/loop/tick.log 2>&1
# or, foreground
watch -n 900 ./.claude/skills/orchestrate/tick.sh
```

## Lane map — what makes a lane single-writer

`orchestrate.config.json` partitions work by lane; `lib/partition.mjs` enforces it in code.
A lane is single-writer because two concurrent items in it would both edit the file named
below — the registration table, schema or index that every change in that area touches.
Collisions cannot surface at merge if colliding lanes are never *assigned* at once.

| Lane | Owns | Forces serialization |
| --- | --- | --- |
| `core` | `multiview-cli/src/{pipeline,run,control}.rs`, `multiview-engine/src/{runtime,drive,clock}.rs`, `multiview-events/src/event.rs`, `multiview-config/src/schema.rs` | `pipeline.rs` (the drive seam — 7 divergent blobs in the 2026-06-16 incident); `schema.rs`; `event.rs` |
| `core-types` | `multiview-core/**` | `core/src/{lib,frame,traits,color}.rs` — all 22 dependent crates rebuild, so it also ripples into every other lane in the wave |
| `api` | `multiview-control/src/{routes/mod,openapi,openapi_schemas,state,lib}.rs`, `docs/api/openapi.json`, auth/session/RBAC | `routes/mod.rs` route table; `openapi.rs`; `state.rs`; the generated `openapi.json` is CI staleness-gated |
| `wrtc` | `multiview-webrtc/**`, `multiview-preview/src/whep*`, `control/src/routes/whip*` | `webrtc/src/transport/mod.rs`; `session.rs` (one `[::]` socket session table shared by WHIP ingest / WHEP serve / WHIP push) |
| `input` | `multiview-input/**`, `multiview-rist-sys/**` | `input/src/lib.rs` (24 cfg-gated `pub mod`), `input/Cargo.toml` `[features]` |
| `preview` | `multiview-preview/**` minus WHEP transport | `preview/src/tap.rs` (the refcounted lazy-start tap registry), `encode.rs` (shared encoder pool) |
| `engine` | `multiview-engine/**` minus runtime/clock/drive, `multiview-hal/src/load.rs` | `engine/src/lib.rs`, `supervisor.rs` task registration |
| `gpu` | `multiview-compositor/**`, `multiview-framestore/**`, `multiview-ffmpeg/**`, `multiview-hal/src/select.rs` | **scoped** — one writer per crate; no file is shared across them (framestore depends only on core; ffmpeg is the FFI leaf) |
| `audio` | `multiview-audio/**`, `multiview-overlay/**` | **scoped** — one writer per crate; verified no shared source file (overlay depends only on core) |
| `bcast` | `control/src/{nmos,is07}*`, `multiview-output/**` | `output/src/lib.rs` (17 cfg-gated mods), `sink.rs` + `fanout.rs` (encode-once-mux-many, invariant #7) |
| `web` | `web/**` | `web/src/app/router.tsx`, `navigation.tsx`, `src/locales/*/messages.po` (`lingui extract` rewrites all three), generated `src/api/schema.ts` |
| `devices` | zowietek / display-kms / sync / cast / node-enroll, `deploy/**` | `control/src/devices/{mod,driver_registry,registry}.rs` — every driver registers in all three |
| `conspect` | `multiview-licence/**`, `multiview-mesh/**` | mesh depends on licence, so a licence public-type change forces a mesh edit; both land stores in `control/src/state.rs` |
| `telemetry` | `multiview-telemetry/**` | `telemetry/src/lib.rs` mod list |
| `gov` | `.claude/**`, `docs/{decisions,research,runbooks}`, `docs/development/work-schedule.md`, `.github/workflows/**`, build pins | `docs/decisions/README.md` (every ADR appends a row), `.github/workflows/ci.yml`, `AGENTS.md` |

Rules that ride with the map:

- **When two items genuinely need the same lane in one wave, serialize them under one
  owner — do not split the territory.**
- Non-owner lanes file the handler **body** and hand the wiring to the owning lane.
- **Cross-cutting, owned by no lane:** `Cargo.lock` (any dep bump) and the generated
  `docs/api/{openapi,asyncapi}.json`. Any route or event change from any lane lands there,
  and CI staleness-gates them — regenerate with `cargo run --locked -p xtask -- gen-openapi`
  and `gen-asyncapi` in the lane that caused the change.
- Wave size is capped by what one integrator can integrate and review well — **3–5
  concurrent lanes** (`maxWave: 5`).
- Unowned surfaces: `xtask/`, `scripts/`, `examples/`, most of `docs/`, root manifests.
  Treat an item touching only these as `gov`, or as `unknown` — which `partition.mjs`
  coerces to single-writer, which is the safe default.

## Gates a cycle owes here

Autonomy is pace, never a lower bar.

- **Classify first.** `scripts/classify.sh` prints the class; it is a floor you may raise,
  never lower. The gates per class are [engineering.md](../standards/engineering.md).
- **R1+ file changes go in a worktree lane** ([`worktree-lane` skill](../../.claude/skills/worktree-lane/SKILL.md)).
  The root checkout stays a clean mirror of `main`.
- **Cross-vendor review above R0 is mandatory and never self-performed by the authoring
  vendor** ([ADR-G005](../decisions/ADR-G005.md), [`review-wave` workflow](../../.claude/workflows/review-wave.js),
  [codex-review runbook](codex-review.md)). Claude-authored → Codex reviews.
  **Never merge on a `claude-fallback` verdict** — that is fresh-context Claude, not a
  second vendor; hold the PR until Codex auth lands.
- Require **≥1 substantive risk statement**; unanimous bland approval is a yellow flag.
  **Never argue a reproduced finding away**, and re-review only the delta after a fix.
- **Invariant #1 (output clock) and #10 (isolation) are blocking** for any engine or
  data-plane item — a change that risks either is R3: stop, write a design note, add a
  chaos/soak test, get explicit operator approval.

## Merge and integration mechanics

Each of these cost an incident once.

- `Closes #n` is **plain text, never backticked** — GitHub ignores the backticked form.
- **Never `gh pr merge --auto` before green** — it merges immediately.
- **`cancelled` is not a passing verdict.** Neither is a green summary over a skipped suite.
- **Push as the authenticated user**; do not rewrite author identity to merge.
- **Rebase a stale lane onto current `main` before integrating** — every in-flight lane on
  2026-06-16 sat on a stale base and cherry-picks conflicted.
- Find **all** of a lane's commits with `git log origin/main..<lane-HEAD>` and cherry-pick
  them as **individual single commits**, never ranges.
- **Never share a build cache across worktrees, and never point a build dir at `/tmp`** —
  per-lane `/tmp` targets once filled the disk with terabytes. A worktree's own `target/`
  is already isolated. A shared cache can link a sibling's stale artifacts and fake a green
  run, so **rebuild from a clean, isolated `target/` before trusting green**.
- After merge: remove the lane's worktree, `git worktree prune`, delete the branch, then
  `git fetch origin && git pull --ff-only origin main` in the root so the next cycle bases
  on current HEAD.

## Salvage — an orphaned `locked` lane whose owning pid is dead

Never force-remove a `locked` worktree belonging to a **live** session. When the pid is
dead, make the work a readable branch *before* the worktree dies:

```sh
git -C <lane> add -A && git -C <lane> commit -m "wip(salvage): <what> — recovered by the loop"
git branch salvage/<descriptive-name> "$(git -C <lane> rev-parse HEAD)"
git worktree unlock <lane> && git worktree remove --force <lane>
```

Then queue the `salvage/*` branch for rebase and completion inside the owning lane.
[`cleanup-sweep`](../../.claude/workflows/cleanup-sweep.js) produces the prune/keep/salvage
lists; it never acts on them.

## Verify

```sh
node .claude/skills/orchestrate/lib/test.mjs        # 22/22 — partitioning + the DONE gate
node .claude/hooks/test-prefer-native-tools.mjs      # 53/53 — the Bash hook, incl. fail-open
bash -n .claude/skills/orchestrate/tick.sh          # syntax
node -e 'JSON.parse(require("fs").readFileSync("orchestrate.config.json","utf8"))'
./.claude/skills/orchestrate/tick.sh                # disarmed => "not armed (no ACTIVE sentinel)"
```

Both test files are gates, not samples. The hook test asserts that every one of the repo's own
contract commands still runs **and** that no block ever names a tool this harness does not have —
`Grep` and `Glob` do not exist here, so a block that recommends them would strand the agent.

## Disarm / roll back

```sh
touch .claude/loop/STOP        # stop after the current cycle
rm -f .claude/loop/ACTIVE      # disarm entirely; ticks become no-ops
crontab -e                     # remove the tick entry
```

Removing the machinery is `git revert` of the installing commit; there is no runtime state
to unwind — `.claude/loop/` is gitignored and holds only sentinels, a lock and a log.

## Memory

The `memory` MCP (embedded qdrant under `.memory/`) is **single-process — one holder per
clone**. Under scheduled ticks only one cycle runs at a time (`tick.lock`), so the cycle is
the single client while it runs and releases it on exit; a concurrent terminal in the same
clone will fail to connect. See [memory-mcp](memory-mcp.md).
