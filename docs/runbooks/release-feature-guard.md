# Runbook — release-artifact feature guard (`_test-seams`)

**What & why.** A CI guard that fails the build if any **shipped** feature set of the
`multiview` binary enables the `multiview-control/_test-seams` cargo feature. That
feature arms the config-watch **post-probe interpose** seam
(`crates/multiview-control/src/config_watch.rs`), whose fired path **writes the watched
config file** — it exists only for the crate's own integration tests. It is off by
default and excluded from every release preset, but it is an ordinary cargo feature that
an errant preset edit, an explicit `--features _test-seams`, or an `--all-features` build
could turn on for a build that then ships. The guard enforces "never shipped" instead of
re-verifying it by hand ([ADR-G008](../decisions/ADR-G008.md); the seam and its accurate
guarantees are [ADR-W027](../decisions/ADR-W027.md)).

**The check.** `scripts/check-no-test-seams.sh`. For each shipped feature set it resolves
the crate's feature list with `cargo tree` and fails if `multiview-control` lists
`_test-seams`:

```bash
cargo tree --locked -p multiview-cli --features <set> --prefix none -f '{p} {f}' -e normal,build
```

- `-f '{p} {f}'` prints each package with its **unified** enabled-feature list — read that,
  **not** the `-e features` subtree, which silently omits an empty feature node like
  `_test-seams = []` pulled transitively (that would be a false-green).
- `-e normal,build` excludes dev edges, mirroring a real `cargo build --release` (never a
  test build), so the crate's own `_test-seams`-enabling self dev-dependency isn't counted.
- `--locked` pins the committed `Cargo.lock` (rule 33).
- A built-in **positive control** (resolve `multiview-control` with `_test-seams` on — it
  MUST be detected) fails the guard if its own detection has rotted, so a green result is
  never a tautology.

**Shipped feature sets checked** (kept in `SHIPPED_FEATURES` in the script):

| Feature set | Ships as |
| --- | --- |
| `ffmpeg,linux-vaapi` | `release.yml` — Linux GitHub-Release binary |
| `ffmpeg,apple` | `release.yml` — macOS GitHub-Release binary |
| `ffmpeg,linux-vaapi,web` | `docker.yml` — generic (VAAPI/QSV) OCI image |
| `ffmpeg,nvidia,web` | `docker.yml` — nvidia (CUDA/NVENC) OCI image |
| `nvidia` / `apple` / `linux-vaapi` / `full` | `multiview-cli` umbrella presets (caught before a workflow wires a new string) |

**Where it runs.**

- `.github/workflows/ci.yml` — job `release-feature-guard`, on every PR + push to `main`
  (the merge gate; gated on the `changes` code filter like the other code jobs).
- `.github/workflows/release.yml` — job `guard`, `needs`-blocks the artifact `build`
  (tags are not covered by `ci.yml`).
- `.github/workflows/docker.yml` — job `guard`, `needs`-blocks the image `build`.

`cargo tree` only **resolves** the graph — no compile, no native deps, no GPU — so every
job runs on a plain free runner in ~1 min (toolchain install dominates).

## Verify

```bash
# From the repo root. Exit 0 + "release-feature-guard: OK" when clean.
./scripts/check-no-test-seams.sh

# Prove it actually catches a slip (RED → revert → GREEN):
sed -i 's/^full = \[$/full = [\n    "multiview-control\/_test-seams",/' crates/multiview-cli/Cargo.toml
./scripts/check-no-test-seams.sh   # exits 1, flags the `full` preset
git checkout -- crates/multiview-cli/Cargo.toml
./scripts/check-no-test-seams.sh   # exits 0 again
```

## Respond when it fires

The job prints `::error::shipped feature set '<set>' enables multiview-control/_test-seams`.
The seam must never ship — do **not** silence the guard; remove the enablement.

1. **Find who turned it on** for that feature set:

   ```bash
   cargo tree --locked -p multiview-cli --features '<set>' -i multiview-control -e features
   ```

   The inverted tree shows the path that enabled `_test-seams`. Usual causes:
   - a `multiview-cli` **preset** (`crates/multiview-cli/Cargo.toml`) gained
     `"multiview-control/_test-seams"` (or a transitive feature that forwards it) — remove it;
   - a **workflow** build line added `--features _test-seams` or `--all-features` for a
     shipped artifact (`release.yml` / `docker.yml` / `deploy/Dockerfile*` `CARGO_FEATURES`)
     — drop it; `--all-features` must never build a shipped artifact.

2. **Re-run** `./scripts/check-no-test-seams.sh` until green, then push.

3. If the **positive control** failed instead (`the detector did not flag … even with it
   enabled`), the check itself rotted — the feature was renamed, or `cargo tree`'s output
   format changed. Fix the detection in the script (the `-f '{p} {f}'` line / the
   `seam_token` pattern) before trusting any green result.

## Extend

- **New shipped feature-string** (e.g. a new deploy image variant): add it to
  `SHIPPED_FEATURES` in `scripts/check-no-test-seams.sh` in the **same change** that adds
  the workflow/preset, and note it in the table above.
- **Guard another test-only feature** the same way: the pattern generalizes — add the
  crate/feature and a positive control for it. (Today only `multiview-control/_test-seams`
  exists.)
