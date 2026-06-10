# ADR-W020: Config-file watch — hot-reload the impacted parts through the one apply machinery

- **Status:** Accepted
- **Area:** Web/API stack · config-as-code ↔ engine (invariant #11 live-apply classification)
- **Date:** 2026-06-10
- **Source:** [management-capability-matrix](../research/management-capability-matrix.md);
  builds on ADR-W018 (live source apply), ADR-W019 (live stored-layout apply), ADR-W015 (honest
  apply semantics / `X-Multiview-Apply`), ADR-0035 (actionable health warnings), ADR-W008
  (bounded command bus), and invariants #1/#10/#11

## Context

The instant-apply doctrine covers two of the three ways a deployment's configuration changes:
the WebUI and the REST API both apply through the engine's frame-boundary command drain
(ADR-W018/W019). The third way — **an operator editing the config file on disk** (vi over ssh,
a git pull, an Ansible template) — did nothing until restart, and worse, did so *silently*: the
file is the boot truth, the running engine and the UI stores kept showing the old state with no
indication the file had moved.

Constraints that shape the design:

* The engine must **never falter** on a file change, and an **invalid file must change
  nothing** — the run keeps going on the last-good document, loudly (inv #1, "bad inputs are
  the purpose").
* There must be **one apply machinery with two triggers**: the file watcher must reuse the same
  bounded command bus, the same drain-side appliers, and the same route-side resolve logic the
  API uses — never a parallel reconfiguration path that can drift.
* The watcher lives on the **tokio side** (control plane); nothing it does may block or
  back-pressure the render thread (inv #10).
* Editors do multi-step writes (truncate+write, or write-temp + `rename(2)`); the watcher must
  debounce and follow atomic renames.
* A future server-side "promote to boot config" flow will write this same file and must not
  re-trigger a reload of its own write.

## Decision

### 1. Watch mechanism: a 1 s metadata poll of the path (no new dependency)

The watcher polls the config **path** (re-`stat` each tick, never a held fd — so a
write-temp+`rename` lands as a normal change) at a 1 s cadence and fingerprints
`(len, mtime, inode)`. A change is acted on only when the **same new fingerprint is observed on
two consecutive polls** (the debounce: an editor's multi-write settles before we read). A
transiently missing file (mid-rename `ENOENT`) is "no change yet", not an error; a file that
*stays* missing is reported once as a rejected load.

Why a poll and not the `notify` crate: zero new dependencies (`cargo-deny` surface unchanged),
identical behaviour on Linux/macOS and on network/overlay mounts where inotify is unreliable,
and a 1–2 s apply latency is well inside what a human editing a file perceives as instant. The
poll runs on a control-plane tokio task and costs one `stat` per second.

### 2. Invalid file ⇒ warn loudly, change nothing

On a debounced change the watcher reads the file and runs the **whole-document** pipeline:
`MultiviewConfig::load_from_toml` → `MultiviewConfig::validate()`. Any failure (unreadable,
TOML parse, semantic validation):

* `tracing::warn!` naming the path and the error;
* a `health.warning.raised` event with the new latched code **`config-file-invalid`**
  (severity `warning`, subsystem `config`), published through the engine's drop-oldest
  `EnginePublisher` — the same off-engine emit seam SA-0's capability warning uses — so the
  existing `warning_ingest` mirrors it into the warning store, `GET /api/v1/health` lists it,
  and the WebUI `HealthBanner` shows it;
* the watch-status surface records `last_rejected` (time + reason);
* **no state change anywhere**: baseline document, engine, control stores all keep running on
  the last-good document. The next **valid** apply clears the warning
  (`health.warning.cleared`).

### 3. Valid file ⇒ structural diff against the RUNNING document

The watcher keeps the currently-applied config document in memory (initially the document the
run booted with; **after each successful file apply the new file becomes the baseline**). A
valid load is diffed per-section by the pure `multiview_config::ConfigDiff::between(running,
next)` — exhaustive over every `MultiviewConfig` section: sources (by id:
added/changed/removed), canvas (pinned signal vs cosmetic axes), layout+cells, outputs,
overlays, probes, audio, control, placement, salvos, tally_profiles, walls, devices,
sync_groups, routing, schema_version.

### 4. Apply each diff item through the SAME machinery the API uses

| Diff section | Action (one machinery, two triggers) | Class |
|---|---|---|
| source added/changed, **synthetic** kind | `Command::UpsertSource` on the bounded bus (the ADR-W018 drain + `LiveSourceHub` path the API uses) | Class-1 live |
| source changed, synthetic → non-synthetic kind | `Command::RemoveSource` (stop the stale generator — mirrors the sources route) + requires-restart warning | honest restart |
| source added/changed, **network/decoded** kind | requires-restart warning naming the source (consistent with the API's `X-Multiview-Apply: restart` truth) | restart |
| source removed (any kind) | `Command::RemoveSource` (bound tiles ride their `on_loss` slate; producers torn down off-thread) | Class-1 live |
| layout / cells changed, canvas signal unchanged | the same resolve+solve path as `cmd_apply_layout`: the shared `resolve_layout_document` (extracted from the route, ADR-W019) parses `{canvas, layout, cells}` from the new document, solves + Class-1-gates it, and submits `Command::ApplyLayout { document: Some(...) }` | Class-1 live |
| canvas width/height/fps changed | Class-2 (pinned canvas, ADR-R004): requires-restart warning, layout apply skipped — never silently | restart |
| canvas pixel_format/background/color changed | requires-restart warning ("canvas") | restart |
| outputs / overlays / probes / devices / sync_groups / audio changed | reseed the control-plane stores + requires-restart warning naming the section (no live path exists yet — honest) | restart |
| control / placement / salvos / tally_profiles / walls / routing / schema_version changed | requires-restart warning naming the section (no store is boot-seeded for these; the base document the export overlays is **not** rewritten — the file itself is the durable truth) | restart |

Source commands are submitted **before** the layout command so a layout binding to a
just-added source resolves at the same frame boundary (FIFO bus). A full bus sheds the submit
with a `tracing::warn!` (inv #10 — the watcher never blocks; re-saving the file retries).

### 5. The control-plane stores follow the file (the UI's truth)

For every changed section that has a boot-seeded store (sources, outputs, overlays, probes,
devices, sync_groups, the audio singleton, and the working layout in the layouts repository),
the watcher re-syncs the store to the new document — create/update/delete by id, exactly the
shape `seed_resources` seeds — so the UI reflects the file within the poll interval. Every
store mutation is audit-logged with actor **`config-file`** through the same audit repository
the routes use. The watcher reaches the stores through a clone of the router's `AppState`
(returned by `bind_and_serve`), so there is exactly one set of stores.

### 6. Honest restart surface: one latched warning

Sections that cannot hot-apply raise/refresh a single latched health warning with the new code
**`config-file-requires-restart`** whose message names the accumulated pending sections (the
warning store coalesces on the code). It stays active until restart — the running process
genuinely differs from the file until then — and is intentionally **not** cleared by a later
file write (a revert cannot un-ring the bell for state the engine never adopted).

### 7. Self-write suppression for the promote-to-boot lane

The spawned watcher returns a cloneable `ConfigWatchHandle` exposing `expect_write()`: each
call increments a generation counter. When the watcher sees a (debounced) change while the
counter is positive it consumes one generation and **adopts** the file as the new baseline
(parse + validate, no commands, no reseed, no warnings — the server-side writer already applied
the state it serialized). A failed parse of an expected write is still warned (a buggy writer
must not be silent). The boot-model lane calls `expect_write()` immediately before writing the
file.

### 8. Observable status: `GET /api/v1/config/watch-status`

A small read-only endpoint (role: read) backed by a shared `ConfigWatchStatus` slot on
`AppState`: `{ active, path, last_applied: {at_ms, summary}?, last_rejected: {at_ms,
reason}?, restart_pending: [section] }`. The SettingsPage gains a "Configuration file" card
showing watch active/inactive, the watched path, the last applied/rejected timestamps, and any
restart-pending sections. Registered in the OpenAPI document; the SPA uses the regenerated
typed client.

### 9. Scope

The watcher runs whenever the run serves a control plane (`[control]` present) — that is where
the command bus, drain, stores, and warning surfaces exist; it is spawned for **both** run
paths (software engine and full pipeline) on their until-stopped branches. A run without
`[control]` has no apply machinery and is not watched (boot-truth semantics, unchanged). The
bounded `--ticks` smoke runs are not watched.

## Alternatives considered

* **`notify` (inotify/FSEvents)** — rejected for this slice: a new dependency tree
  (cross-platform native watchers) for latency we do not need; the 1 s poll is simpler, robust
  across renames/mounts, and trivially testable. Revisit only if sub-second file apply ever
  matters.
* **Re-exec / full restart on change** — violates the instant-apply doctrine (outputs would be
  interrupted for Class-1-able changes) and invariant #1.
* **A parallel "file apply" path into the engine** — rejected outright: one apply machinery,
  two triggers. The file watcher submits the very same commands the routes submit and reuses
  the route's resolve logic as a shared function.
* **Diffing serialized JSON** — rejected: the typed per-section diff over `MultiviewConfig`'s
  `PartialEq` fields is exact (e.g. `Fps` cadence equality is by value, so `50/2` vs `25/1` is
  not a canvas change) and yields actionable per-section results.

## Consequences

* An external edit to the running config file now applies live where the API would apply live,
  reseeds the UI stores, and warns honestly where only a restart can apply — and an invalid
  file changes nothing while telling the operator exactly why.
* Two new latched `WarningCode`s (`config-file-invalid`, `config-file-requires-restart`) join
  the catalog (AsyncAPI regenerated); `config-file-invalid` clears on the next valid apply.
* `bind_and_serve` additionally returns the `AppState` so the watcher shares the router's
  stores; `cmd_apply_layout`'s resolve+solve+gate moved into the shared
  `resolve_layout_document` used by both triggers.
* The baseline-becomes-file rule means a file edit that only touches restart-only sections
  still advances the baseline: the diff is consumed, the warning latches, and the file remains
  the durable truth (`GET /config/export` still overlays the live stores onto the boot base
  document; the watcher does not rewrite the base document).
* The watcher is a pure control-plane tenant: a wedged disk, a slow store, or a flooded bus
  degrades to warnings — the output clock never observes any of it (inv #1/#10).
