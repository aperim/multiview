# ADR-W027: Config-file watch fingerprint folds a content hash (supersedes ADR-W020 §1's stat-only fingerprint)

- **Status:** Accepted
- **Area:** Web/API stack · config-as-code ↔ engine (invariant #11 live-apply classification)
- **Date:** 2026-07-11
- **Source:** cross-vendor review of PR #251 (task #7); supersedes the "Known limit (accepted)"
  of [ADR-W020](ADR-W020.md) §1

## Context

[ADR-W020](ADR-W020.md) §1 detects config-file changes by polling the path and fingerprinting
`(len, mtime, inode)`, acting when the same new fingerprint is observed on two consecutive polls.
It recorded a **"Known limit (accepted)"**: a rewrite that preserves all three axes — a
same-length in-place write landing within the filesystem's mtime granularity, or one whose mtime
is restored by tooling — is invisible to the watcher. It rejected hashing the content each poll
"as a per-second read of the whole file for a corner no real writer hits", on the premise that
editors and deployment tools always change the inode (write-temp + `rename(2)`) or advance mtime.

That premise does not hold for several **legitimate** writers:

* a shell redirect (`config > file`, `sed -i`) rewrites the file **in place** — same inode;
* a non-atomic editor that truncates-and-writes in place — same inode;
* a config-management tool that rewrites the file in place.

When such a write is the **same length** as the adopted content and lands within the filesystem's
coarse mtime granularity, all three stat axes alias the already-applied fingerprint. The
`watch_loop` "already applied" short-circuit then drops the edit **without reading its content** —
the new bytes are never applied, silently. This is not theoretical: on the project's devcontainer
`/tmp` it reproduces ~293/300 for a same-length in-place rewrite, and it surfaced as a flaky
control-plane test (`promote_does_not_retrigger_a_file_watch_apply` and its neighbours) whose
"flake" was the defect firing. A test-only workaround (routing the tests through atomic
temp+rename to mint a fresh inode) was **blocked by the cross-vendor review** as masking a real
correctness defect rather than fixing it.

## Decision

Fold a hash of the file's **bytes** into the watch `Fingerprint` (a fourth field alongside `len`,
`mtime`, `inode`). `probe()` reads the file each poll and folds `fingerprint_content_hash(&bytes)`
(`DefaultHasher` — not an adversarial boundary; whoever writes the config already controls it). A
same-length in-place content change is then a **distinct fingerprint**, so it falls through to the
existing read → validate → apply path. The three fingerprint comparison sites use the derived
`PartialEq`, so they become content-aware with no further change.

* **Raw bytes, not `&str`:** a non-UTF-8 file still fingerprints as a change and settles to the
  existing `read_to_string` reject ("the file cannot be read"), which does the UTF-8 gate.
* **Unreadable-but-present:** if the bytes cannot be read this poll (a permissions problem, or a
  delete racing the `stat`), a `CONTENT_UNREADABLE` sentinel is folded so two such polls settle to
  that same reject path rather than aliasing unchanged content.
* **Cost, re-evaluated:** the config file is small and the read rides the blocking pool at the 1 s
  cadence (like the existing settled read), so a per-poll read is negligible — the "per-second
  whole-file read" objection in ADR-W020 §1 is reversed. Even a ~1 MB config hashes in well under
  a millisecond and is served from the page cache.

## Consequences

* A legitimate same-length in-place rewrite now hot-applies like any other edit; detection no
  longer depends on filesystem `stat` granularity or on the writer changing the inode/mtime.
* **Resume / self-write suppression are unperturbed** (ADR-W020 §5–§7): an atomic rename carrying
  identical bytes still adopts-without-applying via the `last_observed == text` arm (inode differs,
  hash matches); a `touch` (mtime moves, bytes unchanged) stays a no-op; an expected server-side
  write (promote) still adopts via its content-paired token.
* ADR-W020 §1's "Known limit (accepted)" is **resolved**; that ADR's Status is annotated to point
  here. The rest of ADR-W020 (invalid-file handling, per-section diff/apply, store follow, restart
  surface, status endpoint) stands unchanged.
* Regression coverage: a deterministic test forces the worst case (an in-place same-length write
  with mtime pinned to the adopted write's and the inode reused) and asserts the edit applies; the
  atomic-rename edits are kept as additional coverage.

## Alternatives considered

* **Keep stat-only, document the limit louder / tell operators to `touch`** — rejected: silently
  dropping a legitimate operator edit is a correctness defect (inv #1, "bad inputs are the
  purpose"), and a cross-vendor panel blocked the equivalent test-only workaround.
* **Read content only when the stat matches the applied fingerprint** — no cost saving (the steady
  state IS the stat-match case, so it would read every poll anyway) and it would not fix the same
  latent aliasing in the rejected-latch; folding the hash into the fingerprint fixes all comparison
  sites uniformly.
* **Switch to `notify` (inotify/FSEvents)** — orthogonal and still rejected per ADR-W020 (a new
  cross-platform dependency for latency we do not need); the 1 s poll + content hash is simpler and
  robust across renames and network/overlay mounts.
