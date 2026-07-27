# ADR-G009: Scheduled ticks replace the self-paced loop; native tools replace Bash file reads

- **Status:** Accepted
- **Area:** Guardrails
- **Date:** 2026-07-27
- **Source:** operator request (Troy Kelly, @troykelly) — "sweep 2: cut token burn, replace the loop", carrying a 7-day measurement of 1,589 agent sessions

## Context

Agent cost was measured across 1,589 sessions over 7 days. The shape of the spend is not
where instruction-file hygiene assumed it was:

- **Cache reads are 59.5% of true cost.** Cache read is prefix size × turn count and
  nothing else — every turn re-sends the whole transcript.
- **The 4% of sessions past 250 turns are 66% of spend**, and the curve is superlinear:
  $0.47/session at 16–40 turns, **$15.68** at 251–600, **$40.70** above 600.
- **Bash is 74.6% of all tool-result bytes** — 3,179 Bash calls against 165 `Read` calls,
  a 19:1 ratio, at a mean result of 1,258 bytes. It is call *volume*, not large outputs.
- Assistant prose is ~30% of payload, thinking ~21%, tool schemas 2.7%, and **instruction
  files 0.4%**. Shrinking documents cannot move this number.

What is true in this repo today: there is **no** `Stop` hook, no `loop-continue.mjs`, no
`loop-supervisor.mjs`, no resume-directive re-injection and no per-session block budget —
those exist in sibling repos, not here. Multiview's self-continuation is three lines: step
⑨ RESCHEDULE of the `orchestrate` skill, and [ADR-G007](ADR-G007.md)'s Autonomy clause
authorising a `ScheduleWakeup`-driven, fully self-paced Conductor. The effect is the same
pattern the measurement indicts — one session that never ends.

The constraints that bind the answer: the class matrix and its gates
([engineering.md](../standards/engineering.md), [ADR-G008](ADR-G008.md)); the cross-vendor
review gate ([ADR-G005](ADR-G005.md)); committed `.claude` machinery with a warn-only hook
([ADR-G006](ADR-G006.md)); and ADR-G007's territory partition, whose disjointness property
is the reason merges stopped colliding and must survive intact.

## Decision

**A cycle is one Claude Code session. Continuity lives in GitHub, never in a context
window.** An external scheduler starts each cycle clean; nothing in the loop may call
`ScheduleWakeup`, return a `block` decision from a `Stop` hook, or re-enter itself
in-context. ADR-G007's **Autonomy clause and step ⑨ are superseded by this ADR**; every
other part of ADR-G007 — single integrator, territory disjointness, cross-vendor review,
the memory substrate fix — stands unchanged.

Concrete shape:

- **`.claude/skills/orchestrate/tick.sh`** is the scheduler's entry point. It takes a
  single-owner lock (reclaiming a dead pid's), runs exactly **one** cycle in a fresh
  `claude -p` process under `ORCH_MAX_SECONDS` (default 3600), and exits. Sentinels in the
  gitignored `.claude/loop/`: `ACTIVE` arms, `STOP` halts after the current cycle, `DONE`
  records convergence. Point cron or a systemd timer at it; never anything that keeps a
  session alive between cycles.
- **The `orchestrate` skill is now repo-agnostic** and reads `orchestrate.config.json` for
  local specifics. Multiview's half — the lane map and the file that forces each lane
  serial, the class-scaled gate, merge mechanics, salvage, memory — moves to
  [docs/runbooks/orchestrate.md](../runbooks/orchestrate.md).
- **Partitioning and the exit gate are pure, tested code, not model judgement.**
  `lib/partition.mjs` assigns a wave from `{ number, lane, scope, blockedBy }` against
  `singleWriterLanes` / `scopedLanes` / `maxWave`, coercing unknown lanes to single-writer;
  `lib/done-gate.mjs` computes DONE and fails **safe** on every invalid input, with the
  dry-cycle requirement floored at 2 so no caller can argue the loop into a one-cycle exit
  and a materiality floor so a pedantic reviewer cannot prevent convergence.
  `lib/test.mjs` is 22 assertions over both.
- **`orchestrate.config.json`** encodes 15 lanes; 13 are single-writer, and only `gpu` and
  `audio` are scoped-per-crate — each verified to share no source file. `maxWave: 5` keeps
  ADR-G007's "what one integrator can integrate and review well".
- **A `PreToolUse` hook on `Bash`** (`.claude/hooks/prefer-native-tools.mjs`) sends
  `cat`/`head`/`tail`/`sed -n M,Np` that name a file to `Read`, and requires
  `rg`/`grep`/`find`/`ls -R` to **bound their own output** — `-l`, `-c`, `-m N`, or a pipe into
  `head`. **This deviates deliberately from the hook the sweep supplied**, which redirected
  searches to `Grep` and `Glob` tools: **neither tool exists in this harness**, verified
  2026-07-27 by direct call (`No such tool available`) and reported independently for subagents
  and for headless `claude -p` — which is exactly what `tick.sh` spawns. A block whose only
  remedy is an uncallable tool is worse than the burn it prevents, so the intent is kept and the
  remedy is changed to one an agent can actually take. `Read` does exist, so the file-read rules
  stand as written. The hook **fails open** on any parse error or unknown shape, and skips
  segments downstream of a pipe, segments whose stdout is piped onward (those bytes never reach
  the transcript), and any segment containing a redirect or remote exec. Escape hatches: a
  `# raw:` prefix, or `NATIVE_TOOL_HOOK=off`. It chains after the existing warn-only
  `enforce-worktree.mjs` rather than replacing it.
- **[AGENTS.md](../../AGENTS.md) gains "Working efficiently"** — batch shell calls, never
  read files through Bash, keep output small, delegate breadth to subagents, do not
  narrate, checkpoint and stop past ~150 turns.

The loop ships **disarmed**. The `status: ready` / `status: blocked` / `loop-state` labels
do not exist yet, and the backlog is still `docs/development/work-schedule.md` rather than
issues; both are operator decisions recorded in the runbook, not defects in the machinery.

## Rationale

- **The loop shape is the whole cost.** At 1,589 sessions the difference between a
  250-turn session and a 40-turn one is $15.21; between a 600+ turn session and a 40-turn
  one, $40.23. Ending the session is the only lever that touches the 66%. Sweep 1 shrank
  instruction files, which are 0.4% of payload — a rounding error against this.
- **Bash volume, not Bash output, is the second lever.** At 19:1 against `Read` and 74.6%
  of tool-result bytes, redirecting the discovery idioms (`cat`, recursive `grep`, `find`)
  to bounded tools cuts both the per-call turn and the bytes that then ride in the prefix
  for every remaining turn. Enforcing it in a hook rather than a document is deliberate:
  documents are 0.4% of payload precisely because they are easy to not read.
- **Failing open is correct for this hook.** A hook that blocks legitimate work is worse
  than one that misses; verification found it blocks 3 of 397 real repo commands, and two
  of those three are exactly the discovery greps it exists to redirect.
- **Determinism where judgement was load-bearing.** ADR-G007's disjointness property was
  enforced by one model's attention. Asking a model "are these safe together?" is how two
  PRs that were green alone land red together. `partition.mjs` makes it arithmetic, and
  unknown lanes coerce to single-writer so an unclassified item can never widen a wave.
- **DONE must be computed.** A self-terminating loop that may declare its own completion
  either stops early on a green CI run or never stops at all. Failing safe on invalid input
  means "I could not confirm" never reads as "finished".

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| **Keep the self-paced `ScheduleWakeup` Conductor** (status quo) | It is the measured pattern: 4% of sessions, 66% of spend, superlinear in turn count. Nothing else in the cost profile is worth attacking first. |
| **Shrink instruction files further** | Instruction files are **0.4%** of request payload. Sweep 1 already did this; repeating it cannot move a 59.5% cache-read cost. |
| **Cap turns per session and let the session resume itself** | A cap without an external starter either strands the work or re-injects a resume directive — which is the burn machinery under another name. The starter has to live outside the session. |
| **Keep the repo-specific 9-step skill and only delete step ⑨** | Leaves the territory partition enforced by model judgement, leaves no config seam for the tested libs, and keeps ~2 KB of Multiview specifics in a file every session loads. The runbook is a better home for the *how*. |
| **Supersede ADR-G007 outright** | Its territory partition, sole-integrator rule, cross-vendor gate and memory substrate fix are all still correct and load-bearing; only the pacing mechanism is wrong. A wholesale supersede would orphan the citations in `data-plane-safety.md` and `engineering.md`. |
| **Make the Bash hook fail closed, or allowlist commands** | Fails the "worse than one that misses" test — a fail-closed parser bug halts all shell work, and an allowlist rots against a 397-command surface across CI, scripts, xtask and runbooks. |
| **Ban `Bash` for file reads by policy only** | Policy lives in instruction files, which are 0.4% of payload and demonstrably not binding — the 19:1 ratio was measured under existing policy. |

## Consequences

- **Easier:** cost is bounded per cycle rather than compounding across a session; a crashed
  or timed-out cycle costs one tick because state is in GitHub; wave partitioning and the
  exit gate are unit-testable and were tested (22/22); the skill is portable across repos.
- **Harder / committed to maintaining:** `orchestrate.config.json` must track the crate
  layout — a new crate outside a lane becomes `unknown`, which is safe but serial, so lanes
  need reviewing when crates are added. The runbook is now the only home for the lane map
  and must be updated in the same change that moves a territory. Cross-cycle continuity
  depends on the loop-state issue being accurate; `gh issue list` lags creates by ~a minute,
  so the reservation ledger is written **before** the create.
- **Operationally open:** the loop cannot dispatch until the three labels exist and the
  backlog is represented as issues. It is armed only by an explicit
  `touch .claude/loop/ACTIVE`.
- **Verified against the repo's own command surface:** 397 distinct commands were extracted
  from CI, the devcontainer, `scripts/`, `xtask`, `web/package.json`, the runbooks and the agent
  docs, and replayed through the hook. **394 pass.** The 3 blocked are all unbounded `rg`
  examples in `working-in-this-monorepo.md` — exactly what the hook exists to stop — and that
  caller was fixed to bound them. `.github/workflows/gitleaks.yml`'s
  `grep "${tarball}" checksums.txt | sha256sum -c -` passes because its stdout is consumed by a
  pipe; that security control is untouched, and CI never sees the hook in any case — it only
  intercepts agent `Bash` calls.
- **Every block must leave an action available.** That is the standing constraint on this hook:
  if a future harness drops `Read` too, the file-read rules must be re-adapted rather than left
  pointing at something uncallable.
- **Invariants:** none touched. This is agent policy — **R2** by the class matrix — not a
  data-plane change; invariants #1 and #10 are unaffected and their blocking status at
  R3 is unchanged.
