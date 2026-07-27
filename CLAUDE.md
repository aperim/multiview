@AGENTS.md

[`AGENTS.md`](AGENTS.md) is canonical. This file adds only Claude-Code specifics.

- **Lanes.** File-changing work at R1 and above goes in a worktree lane (`worktree-lane` skill);
  the root checkout stays a clean mirror of `main`. The `PreToolUse` hook warns, never blocks
  (ADR-G006). R0 prose batches may be edited in place.
- **Skills** — `worktree-lane` (start of a file-changing task) · `adr` (a decision that constrains
  future changes; R2/R3 only) · `memory` (`qdrant-find` before non-trivial work, `qdrant-store`
  when you learn something a future session needs) · `orchestrate` (driving the backlog as the
  Conductor — [ADR-G007](docs/decisions/ADR-G007.md)).
- **Workflows** in [`.claude/workflows/`](.claude/workflows/) — `review-wave` (the cross-vendor
  gate; never self-performed by the authoring vendor) · `wave-fanout` · `orient` · `cleanup-sweep`.
- **Subagents.** Delegate wide reading and mechanical edits; they return conclusions, not
  transcripts. Tell each one its lane path explicitly and have it `cd` there in every Bash call — a
  background agent's shell can otherwise run against the root checkout and misreport lane state.
- **Nested `CLAUDE.md`** files (per crate, `web/`, `docs/runbooks/`) load when you read a file in
  that directory and are **not** re-injected after `/compact`. If a crate rule seems to vanish
  mid-session, read any file in that crate to reload it.
- **MCP** — one server: `memory` (local Qdrant under `.memory/`, single-process; one session per
  clone at a time). Runbook: [memory-mcp](docs/runbooks/memory-mcp.md).
