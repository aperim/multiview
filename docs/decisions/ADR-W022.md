# ADR-W022: Boot / Loaded / Running configuration model — resume, revert-to-start, promote-to-boot

- **Status:** Accepted
- **Area:** Web/API stack · config-as-code lifecycle (invariant #11 live-apply classification)
- **Date:** 2026-06-11
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md);
  builds directly on ADR-W020 (config-file watch + `expect_write()` self-write suppression),
  ADR-W015 (config export composition), ADR-W018/W019 (the one live-apply machinery),
  and invariants #1/#10/#11

## Context

After ADR-W020 a deployment's configuration moves through three independent triggers — the
WebUI/REST API, the file watcher, and restarts — but the process has **no model of where it
started, what it is running now, and what the file on disk says**. Concretely:

* A crash or power cut loses every live change since boot; there is no machine-written record
  of the running state to recover from.
* An operator who has drifted mid-show ("we changed six things during the broadcast") has no
  one-button way back to the known-good state the show **started** with — especially if the
  boot file itself was edited mid-show.
* The "edit live, then export, then scp the file back" loop (ADR-W015) works but is manual;
  the `expect_write()` suppression seam ADR-W020 §7 built for a server-side promote flow has
  **no caller**.
* The UI shows live state as if it were the durable truth; there is no honest indicator that
  the running configuration has diverged from the boot baseline.

## Decision

### 1. The model (pinned)

> - **Boot** = the config file `multiview run` was started with: the deliberate cold-start
>   baseline; the watched + hand-edited file (the ADR-W020 watcher already watches it).
> - **Loaded** = an immutable snapshot of Boot taken at process start, kept in memory AND
>   persisted to disk (`<config-dir>/.multiview/loaded.toml`, atomic write at startup) — the
>   "recover to exactly where it was on start" target even if the boot file was edited
>   mid-show.
> - **Running** = Loaded + every live change since (API/UI edits, file-watch applies).
>   Continuously persisted — debounced (~2 s), atomic rename, machine-written, NEVER
>   watched — to `<config-dir>/.multiview/active.toml`. Compose source: the SAME document
>   composition `GET /api/v1/config/export` uses (`compose_export_document` in
>   `crates/multiview-control/src/routes/config.rs`) — extract/reuse, do not duplicate.
>   Trigger: any successful resource/layout mutation + file-watch apply — ONE choke point
>   (the audit recorder), not sprinkled call sites.
> - **Cold-start policy**: `[control] start = "boot" | "resume"` (serde default `"boot"`;
>   the token is validated). `"resume"`: if `active.toml` exists, parses, AND validates,
>   load IT as the starting Running state (the boot file stays the watch target);
>   invalid/missing active → fall back to boot with a warning.
> - **Actions** (REST, OpenAPI-annotated, RBAC write, `Idempotency-Key` like other
>   commands, audited):
>   - `POST /api/v1/config/revert-to-start` — Running := Loaded, applied LIVE through the
>     one apply machinery (ADR-W020's diff→action application:
>     `diff(current-running-doc, loaded-doc)` → the same command paths; restart-only
>     sections get the same honest warnings). `202` + per-section applied/warned summary.
>   - `POST /api/v1/config/promote` — write the current Running document to the BOOT file
>     path (server-side), using the watcher's `expect_write()` suppression seam (this is
>     its caller) + a config-versioning commit + audit. Confirm-required in the UI.
> - **UI honesty**: a persistent divergence indicator with **Revert to start** (confirm,
>   destructive styling) and **Promote to boot** (confirm; explains it rewrites the config
>   file) actions.

`<config-dir>` is the directory containing the boot config file; the state directory is
`<config-dir>/.multiview/` (created on demand). Both `loaded.toml` and `active.toml` are
**machine-written canonical TOML** (`MultiviewConfig::to_toml` of the parsed, validated
document — comments in the hand-authored boot file are not carried; these are state files,
not authoring surfaces). The watcher never watches them, and `validate` runs on every write
path, so an `active.toml` that exists always round-trips `MultiviewConfig::validate`.

### 2. The apply machinery moves to `multiview-control` (one machinery, three triggers)

ADR-W020's diff→action application (`apply_diff` + the store resyncs + the shared
`resolve_layout_document` call path) lived in `multiview-cli/src/config_watch.rs`, which the
revert route cannot reach (the dependency arrow is `cli → control`). The whole
`config_watch` module **moves verbatim into `multiview_control::config_watch`** (the CLI
keeps a re-export shim so `multiview_cli::config_watch::*` paths — including the existing
ADR-W020 integration tests — compile unchanged). The apply core becomes the public
`apply_document_diff(state, actor, &diff, &next) -> ApplyOutcome { parts, restart }`, with
the audit **actor** a parameter: the watcher passes `config-file`, the revert route passes
the authenticated principal. There is exactly one diff→apply implementation; the watcher and
the revert route are two callers of it (the routes were already the third trigger via their
own command submissions).

### 3. Running persistence: one choke point, debounced, fail-soft

* **Choke point:** `AppState::audit()` — every successful mutation in the control plane
  (resource CRUD, accepted commands, file-watch resyncs, revert applies) already records an
  audit entry there. After recording, it now also fires `running_changed`
  (`tokio::sync::Notify` on `AppState`): a one-permit, coalescing signal that can never
  queue, grow, or block (inv #10). No call site changes; future mutating routes inherit the
  trigger by construction.
* **Persister:** a control-plane tokio task (`boot_model::spawn_running_persist`): waits on
  `running_changed`, sleeps the debounce (~2 s, bounded — at most one write per debounce
  window), then composes the running document via the SAME `compose_export_document` the
  export route uses, deserializes + `validate()`s it, renders TOML, and writes
  `active.toml` atomically (same-directory temp file → `fsync` → `rename(2)` → directory
  `fsync`). Any failure — a store fault, a document that does not compose, an I/O error —
  is a `tracing::warn!` and a skipped write; the task never exits on error and nothing else
  is touched (fail-soft). The file I/O rides `spawn_blocking` (review m1: the fsync'd write
  never parks the control-plane reactor; the boot-model status route's file read rides
  `tokio::fs` for the same reason). At startup the starting Running state is persisted once
  (so a stale `active.toml` from a previous run can never outlive the run that supersedes
  it), and at graceful teardown — reached on Ctrl-C **and SIGTERM** (review m2: `docker
  stop`/systemd) — `finish_running_persist` aborts the task, **awaits its termination**
  (review M2: the deterministic `.tmp` is single-writer only if the final persist can never
  overlap a write the dying task is still finishing on a blocking thread), then runs one
  final best-effort persist capturing changes younger than the debounce.
* **Audit-trigger over-approximation, accepted:** some audit entries (e.g. a config-revision
  commit, an alarm ack) do not change the composed document; the debounced persister then
  rewrites an identical `active.toml`. That costs one small atomic file write per ~2 s worst
  case and keeps the trigger a single, future-proof choke point.

### 4. Cold-start resume

`ControlConfig` gains `start: StartMode` (`boot` | `resume`; serde-typed, so an unknown
token fails parse — validated by construction; default `boot`). The **boot** file's policy
decides. On `resume`, the run reads `<config-dir>/.multiview/active.toml`; if it reads,
parses, and validates, that document becomes the starting Running state — with the
**storeless restart-only sections spliced from the BOOT document** (review M1: `control`,
`placement`, `salvos`, `tally_profiles`, `walls`, `routing`, `schema_version` have no
control store; the boot file is their durable truth and a restart is exactly when they
take effect, so a boot-file `[control] listen` edit lands on the restart the operator
performed instead of losing to the stale machine-written copy). The spliced document is
re-validated; a combination that no longer validates falls back to boot with the reason
surfaced. The engine is built from the result, the control stores are seeded from it, the
export base document is it, and the ADR-W020 watcher's baseline is it — while the watcher
keeps watching the BOOT path, so an external boot-file edit during a resumed run still
hot-applies (the diff is computed against the resumed baseline). The watcher's
**last-observed content is seeded with the boot-load text** (review m4): a settled
observation whose content still equals it — the unchanged boot file under a resume, or a
touch/identical rewrite — is adopted without applying, so the resumed state is never
clobbered by a file that did not actually change; an edit landing in the boot window
differs from that text and still applies (the ADR-W020 review-M2 semantics hold). A
missing/unreadable/invalid `active.toml` falls back to the boot document with a
`tracing::warn!` naming the reason (also surfaced on `GET /api/v1/config/boot-model` as
`resume_fallback`). **Loaded stays the boot snapshot in both modes** (the model pins it:
revert-to-start targets the deliberate cold-start baseline, not the resumed state).

### 5. `POST /api/v1/config/revert-to-start` (role: write, `Idempotency-Key`, audited)

Running := Loaded, live: compose the current Running document (the export composition),
deserialize it, `ConfigDiff::between(running, loaded)`, and hand the diff to the one
`apply_document_diff` — synthetic source adds/edits/removals ride
`UpsertSource`/`RemoveSource` on the bounded bus, a layout/cells delta rides the shared
resolve+solve+Class-1 gate and `ApplyLayout`, stores resync to the Loaded values (audited
under the requesting principal), and restart-only sections are reported honestly in the
response's `restart_only` (in the common boot-start case the engine never adopted those
sections' drift, so reverting their stores actually re-converges doc and engine; after a
resume with a different canvas the Class-2 hold applies exactly as in ADR-W020). The
response is `202` with the per-section summary. An empty diff returns `202` with
`reverted: false` and applies nothing. The response is **shed-aware** (review M4):
`reverted: true` with the full summary only when every engine command landed on the
bounded bus; a shed apply answers `202` with `reverted: false`, the `shed` count, the
partial summary, and raises the `config-file-apply-incomplete` warning with
revert-specific remediation — unlike the watcher nothing retries a revert, so the operator
is told to retry once the bus drains (the stores already hold the start values). A
composition failure (`422`) releases the `Idempotency-Key` reservation so a corrected
retry with the same key actually runs. The ADR-W020 watcher's *file* baseline is
deliberately untouched — its baseline tracks the last applied **file** content (W020
semantics; API edits already moved Running without moving it), and the latched
`config-file-requires-restart` warning stays honest because the file still differs from
what the engine adopted.

### 6. `POST /api/v1/config/promote` (role: write, `Idempotency-Key`, audited, UI-confirmed)

Write the current Running document to the **boot file path**, server-side: compose →
deserialize → `validate()` → `to_toml()` → `expect_write()` on the installed watch handle
(ADR-W020 §7 — this is the seam's first and intended caller; the token is
**content-paired**, so an unrelated external edit landing inside the settle window is
still applied, never adopted) → atomic write to the boot path → a config-versioning commit
(target **`boot`**, the promoted JSON document, the principal, message `promote running
configuration to boot`) → audit. Returns `200` with the written path, byte count, and the
committed revision id. With no watcher installed (store-only deployments) the suppression
step is skipped — there is nothing to suppress. A **failed write releases the banked
token** (review B1: a leaked token would silently adopt a later real edit carrying the
same content) and releases the idempotency reservation. The atomic write **preserves the
destination's mode** (review M3: a chmod-600 boot file stays 600 across the temp-file +
rename), and the boot path is canonicalized at startup so a symlinked config is promoted
at its real file — the rename replaces the file, never the symlink.

### 7. Observability: `GET /api/v1/config/boot-model` (role: read)

A small read-only endpoint backing the UI indicator: whether a boot model exists
(`modeled`), the boot path, the `start` policy, whether this run `resumed` (+
`resume_fallback` reason), the per-section divergence of Running from **Loaded**
(`diverged_from_loaded`, computed by the same pure `ConfigDiff` — exact, cheap at human
poll rates) and from the **current boot file** (`diverged_from_boot_file`, `null` +
`boot_file_error` when the file is unreadable/invalid), and the last successful
`active.toml` write time. The UI shows section **names**, not a count: the originally
floated "Boot + N changes" count is not cheaply truthful (the audit ring is bounded at
10 000 and one logical apply records several entries), while the per-section diff is exact
and more actionable — this is the "divergence boolean + why" fallback the model allows,
strengthened to per-section names.

**UI:** the SettingsPage configuration area gains a "Boot configuration" card (chosen over
a new app-shell element: the config-file watch card already lives there, the page is the
configuration surface, and the card is testable without new shell plumbing): the divergence
lines, **Revert to start** (destructive styling + confirm dialog) and **Promote to boot**
(confirm dialog that states it rewrites the configuration file on the server). All strings
Lingui-wrapped. A rendered diff view is deferred: the config-versioning diff endpoint
compares two stored revisions, not "live composition vs snapshot", so it is not nearly-free
— follow-up, not silently dropped.

### 8. Scope and isolation

Everything in this ADR is control-plane tenancy on tokio: composition reads read-mostly
stores, persistence writes files, applies ride the existing bounded non-blocking bus and the
drop-oldest publisher. The render thread never sees any of it (inv #1/#10). Like the
ADR-W020 watcher, the boot model exists only when the run serves a control plane from a
config file; `--ticks` smoke runs and store-only deployments honestly report `modeled:
false` and refuse the actions with a problem document.

## Alternatives considered

* **Trigger persistence from every mutating call site** — rejected: ~30 call sites today
  and a guarantee that future routes forget one. The audit recorder is the one funnel every
  successful mutation already passes through.
* **Watch `active.toml` for changes** — rejected outright: it is machine-written state, not
  an authoring surface; watching it would loop the writer into the reader.
* **Persist Running as raw JSON of the stores** — rejected: `active.toml` must be a valid
  boot-shaped document (`load_from_toml` + `validate` round-trip) so resume can treat it
  exactly like a config file; the export composition already guarantees that shape.
* **A "Loaded follows resume" variant (revert returns to the resumed state)** — rejected:
  the model pins Loaded to the Boot snapshot; the deliberate cold-start baseline is the
  revert target an operator reasons about mid-show.
* **An exact "N changes" counter** — rejected for honesty (see §7): bounded audit ring +
  multi-entry applies make N unreliable; the per-section diff is exact and cheaper to trust.
* **Promote via the client (export → upload)** — exists already (ADR-W015) and stays; the
  server-side promote removes the lossy manual hop and is the `expect_write()` seam's
  designed consumer.

## Consequences

* A power cut no longer loses the show state: `start = "resume"` brings the process back to
  the last persisted Running state within the debounce window, while `loaded.toml` always
  preserves the cold-start baseline for forensics and revert.
* The `config_watch` module (and its `ConfigWatchHandle`) now lives in `multiview-control`;
  `multiview-cli` re-exports it. `AppState` grows `boot_model`, `running_changed`, and the
  installed watch handle; `bind_and_serve` accepts the optional boot model.
* Three new endpoints join the OpenAPI document (`revert-to-start`, `promote`,
  `boot-model`); the SPA client is regenerated.
* `active.toml` fidelity equals export fidelity by construction: anything the export
  composition does not capture (e.g. a bare `swap` re-point that only mutates the engine's
  working binding) is missing from Running persistence too — one composition to fix, both
  surfaces improve (known ADR-W015/W019 limitation, unchanged here).
* Sections without control stores (`control`, `placement`, `salvos`, `tally_profiles`,
  `walls`, `routing`) cannot hot-revert; revert reports them `restart_only` and the
  indicator keeps naming them until restart.
