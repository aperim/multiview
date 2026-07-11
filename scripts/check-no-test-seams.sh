#!/usr/bin/env bash
# Release-artifact guard (ADR-G008, task #109): assert that NO shipped build of
# the `multiview` binary enables the `multiview-control/_test-seams` cargo
# feature.
#
# `_test-seams` arms the config-watch post-probe interpose test seam
# (crates/multiview-control/src/config_watch.rs). Its fired path WRITES the
# watched config file — it exists ONLY so the crate's own integration tests can
# drive a same-inode in-place rewrite of the config. It is off by default and
# excluded from every release preset (verified), but it is an ordinary cargo
# feature: an errant preset edit, an explicit `--features _test-seams`, or a
# workspace `--all-features` build could still turn it on. This guard fails if
# that ever happens for a feature set that ships.
#
# Why a bespoke guard and not an existing tool:
#   * `cargo deny` bans CRATES/licenses/advisories, not enabled FEATURES.
#   * There is no reliable compile-time "this is a release build" cfg to gate on.
# So we resolve the feature graph with `cargo tree` and assert the seam feature
# is absent. See docs/runbooks/release-feature-guard.md for the response drill.
#
# Runs in: .github/workflows/{ci,release,docker}.yml. Cheap — `cargo tree` only
# RESOLVES the graph (no compile, no native deps, no GPU), so it runs anywhere.
set -euo pipefail

# Run from the repo root regardless of the caller's CWD (script lives in scripts/).
cd "$(dirname "$0")/.."

readonly SEAM_FEATURE='_test-seams'
readonly CRATE='multiview-control'
readonly PKG='multiview-cli' # the shipped `multiview` binary aggregates all features

# The exact feature sets that produce a SHIPPED artifact, plus every umbrella
# release preset — so a NEW shipped feature-string is caught even before a
# workflow wires it. Keep in sync with the release/docker workflows and the
# `multiview-cli` presets (the runbook explains how).
readonly -a SHIPPED_FEATURES=(
  "ffmpeg,linux-vaapi"     # release.yml — linux GitHub-Release binary
  "ffmpeg,apple"           # release.yml — macOS GitHub-Release binary
  "ffmpeg,linux-vaapi,web" # docker.yml  — generic (VAAPI/QSV) OCI image
  "ffmpeg,nvidia,web"      # docker.yml  — nvidia (CUDA/NVENC) OCI image
  "nvidia"                 # multiview-cli umbrella preset
  "apple"                  # multiview-cli umbrella preset
  "linux-vaapi"            # multiview-cli umbrella preset
  "full"                   # multiview-cli everything-non-GPL preset
)

# Detection. `cargo tree -f '{p} {f}'` prints, per resolved package, the crate
# followed by its UNIFIED enabled-feature list for THIS build, e.g.
#
#   multiview-control v0.1.0 (/…/crates/multiview-control) default,openapi,_test-seams
#
# We must read that {f} list rather than the feature SUBTREE: a plain
# `cargo tree -e features` silently omits the node for an EMPTY feature such as
# `_test-seams = []` that is pulled transitively (verified), which would make the
# guard a false-green. Flags:
#   --prefix none      drop the tree glyphs so each crate line anchors at column 0
#   -f '{p} {f}'       print each package with its resolved feature list
#   -e normal,build    exclude dev edges — mirror a real `cargo build --release`,
#                      never a test build (so the crate's own `_test-seams`-
#                      enabling self dev-dependency is not traversed)
#   --locked           pin the committed Cargo.lock (deterministic; rule 33)
#
# A crate line is `multiview-control v… (path) f1,f2,…`; the feature token is
# delimited by a space (after the path) or commas, so match `[ ,]_test-seams(,|$)`.
readonly tree_flags=(--prefix none -f '{p} {f}' -e normal,build --locked)
readonly seam_token="[ ,]${SEAM_FEATURE}(,|\$)"

# Print the resolved crate line(s) for one (package, feature-set) on stdout.
# Captures `cargo tree` first and `|| return`s its exit code on failure, so a
# resolution FAILURE is never mistaken for "seam absent" (fail closed; rule 37);
# the trailing `|| true` only absorbs grep's no-match, not a cargo error.
crate_line() {
  local pkg="$1" feats="$2" out
  out="$(cargo tree "${tree_flags[@]}" -p "$pkg" --features "$feats")" || return
  printf '%s\n' "$out" | grep -E "^${CRATE} " || true
}

fail=0

echo "release-feature-guard: checking ${#SHIPPED_FEATURES[@]} shipped feature set(s) for ${CRATE}/${SEAM_FEATURE}"
for feats in "${SHIPPED_FEATURES[@]}"; do
  if ! line="$(crate_line "$PKG" "$feats")"; then
    echo "::error::cargo tree failed to resolve shipped feature set '${feats}' — the guard cannot verify it; failing closed."
    fail=1
    continue
  fi
  if grep -Eq "$seam_token" <<<"$line"; then
    echo "::error::shipped feature set '${feats}' enables ${CRATE}/${SEAM_FEATURE} — the config-file-writing config-watch test seam must NEVER ship. See docs/runbooks/release-feature-guard.md."
    fail=1
  else
    echo "ok: '${feats}' does not enable ${CRATE}/${SEAM_FEATURE}"
  fi
done

# Positive control — prove the detector actually fires. Resolve the crate with
# the seam feature explicitly ON; its line MUST contain the token. If it does
# not, the check above has silently rotted into an always-green no-op (feature
# renamed, cargo-tree format changed, pattern drift), so fail loudly rather than
# ship a tautological guard (rules 19/25).
if ! pc_line="$(crate_line "$CRATE" "$SEAM_FEATURE")"; then
  echo "::error::positive control could not resolve ${CRATE} with ${SEAM_FEATURE} enabled — cannot self-verify the detector; failing closed."
  fail=1
elif grep -Eq "$seam_token" <<<"$pc_line"; then
  echo "ok: positive control detects ${CRATE}/${SEAM_FEATURE} when it is explicitly enabled"
else
  echo "::error::positive control FAILED — the detector did not flag ${CRATE}/${SEAM_FEATURE} even with it enabled. The guard's detection has rotted; fix it before trusting a green result."
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "release-feature-guard: FAILED"
  exit 1
fi
echo "release-feature-guard: OK — no shipped feature set enables ${CRATE}/${SEAM_FEATURE}"
