# ADR-G008: Release-artifact feature guard — no shipped build enables `multiview-control/_test-seams`

- **Status:** Accepted
- **Area:** Guardrails
- **Date:** 2026-07-11
- **Source:** cross-vendor review of PR #251 (ADR-W027), finding B(2) — team-lead-approved follow-up (task #109)

## Context

[ADR-W027](ADR-W027.md) added a test-only cargo feature, `multiview-control/_test-seams`,
that arms the config-watch **post-probe interpose** seam in
`crates/multiview-control/src/config_watch.rs`. When armed, that seam's fired path
**writes the watched config file** — it exists solely so the crate's own integration
tests can deterministically drive a same-inode in-place rewrite of the config between the
watcher's probe and apply. The seam is gated `#[cfg(feature = "_test-seams")]`, is off by
default, is inert unless a caller explicitly arms it via `with_post_probe_interpose`
(no production code does), and is excluded from every `multiview-cli` release preset.

That is a defence-in-depth situation, not a proof. `_test-seams` is an ordinary cargo
feature: an errant edit that adds it to a preset, an explicit `--features _test-seams`,
or a workspace `--all-features` build could turn it on for a build that then ships. A
shipped binary carrying a config-file-writing seam is a real integrity risk. The rule-27
review of PR #251 verified — via `cargo tree -p multiview-cli --features full` — that no
preset pulls it *today*, but asked for that fact to be **enforced**, not just observed.
The binding constraints are the guardrail rules (rule 6 no-partial-ship, rule 27 no
aspirational claims, rule 33 deterministic `--locked` builds, rule 42 runbook-as-you-work)
and the deploy surface (rule 40): the GitHub-Release binary (`release.yml`) and the GHCR
OCI images (`docker.yml`).

## Decision

**A CI guard asserts that no shipped feature set of the `multiview` binary enables
`multiview-control/_test-seams`, and fails the build if one does.** The check is
`scripts/check-no-test-seams.sh`: for each shipped feature set it runs
`cargo tree --locked -p multiview-cli --features <set> --prefix none -f '{p} {f}' -e normal,build`
and fails if the resolved `multiview-control` line lists `_test-seams`. A built-in
**positive control** (resolving `multiview-control` with the feature explicitly on, which
must be detected) keeps the check from silently rotting into an always-green no-op.

Shipped feature sets checked: `ffmpeg,linux-vaapi` and `ffmpeg,apple` (release.yml
binaries); `ffmpeg,linux-vaapi,web` and `ffmpeg,nvidia,web` (docker.yml images); and the
`nvidia` / `apple` / `linux-vaapi` / `full` umbrella presets (so a *new* shipped
feature-string is caught before a workflow wires it).

The script gates three workflows: a `release-feature-guard` job in `.github/workflows/ci.yml`
(every PR + push to `main` — the merge gate) and a `guard` job that `needs`-blocks the
artifact build in `.github/workflows/release.yml` and `.github/workflows/docker.yml` (the
tag/main artifact paths, which `ci.yml` does not cover). The operational how — including
the response drill when it fires — is [`docs/runbooks/release-feature-guard.md`](../runbooks/release-feature-guard.md).

## Rationale

- `cargo tree` **resolves** the feature graph without compiling — no native deps, no GPU,
  seconds per preset — so the guard runs on any free runner and needs no FFmpeg/CUDA/VAAPI.
- Reading each package's unified feature list via `-f '{p} {f}'` is robust where a plain
  `-e features` render is not: cargo-tree **omits the subtree node for an empty feature**
  like `_test-seams = []` when it is pulled transitively, which would make a naive grep a
  false-green. The `{f}` list reflects the exact feature set cargo compiles the crate with.
- `-e normal,build` excludes dev edges, mirroring a real `cargo build --release` (never a
  test build), so the crate's own `_test-seams`-enabling self dev-dependency is correctly
  not counted. `--locked` keeps resolution deterministic against the committed `Cargo.lock`.
- The positive control makes the guard a genuine RED→GREEN check (rules 18/25), not a
  tautology: it fails loudly if the feature is renamed or cargo-tree's format drifts.

## Alternatives considered

| Alternative | Rejected because |
| ----------- | ---------------- |
| **`cargo deny`** | Bans crates / licenses / advisories / sources — it has no concept of an *enabled feature*, so it cannot express "feature X of crate Y must be off." |
| **A compile-time `cfg` gate** (`#[cfg(not(feature = "_test-seams"))] compile_error!` in a release cfg) | There is no reliable "this is a release build" cfg to key on; `--release` is a profile, not a cfg, and the seam must stay compilable for tests. |
| **`cargo build --unit-graph`** (exact per-unit features) | Requires `-Z unstable-options` (nightly); the toolchain is pinned stable (MSRV 1.85, rule 39). |
| **Grep the built binary for the seam symbol** | Needs a full native release build (FFmpeg/codecs) and is defeated by symbol stripping — expensive and unreliable versus resolving the graph. |
| **Trust the manual `cargo tree --features full` check from the PR #251 review** | Rule 27: an observed-once fact is not an enforced one; a future preset edit or an errant `--all-features` release would regress it silently. |

## Consequences

- **Easier:** a preset edit or workflow change that would ship the config-file-writing
  seam now fails CI at PR time and is blocked from every release/image artifact; the
  no-`_test-seams` guarantee is enforced, not re-verified by hand each time.
- **Harder / committed to maintain:** the shipped-feature-set list in the script must
  track the release/docker workflows and the `multiview-cli` presets. The runbook says
  how, and the list deliberately includes the umbrella presets so a preset gains coverage
  before a workflow wires it; a genuinely new *shipped* feature-string (a new deploy image
  variant) must be added to the list in the same change that introduces it.
- **Cost:** three short CI jobs (~1 min each: toolchain install dominates; the `cargo tree`
  resolves are seconds). No product-code change; touches only `scripts/`, `.github/workflows/`,
  and `docs/`. Free-tier only (rule 36).
- **Scope:** the guard concerns build-time feature reachability; it does not touch the
  data plane, so invariants #1/#10 are unaffected. It complements — does not replace —
  the structural gating and the accurate ADR-W027 claims that keep the seam off by default.
