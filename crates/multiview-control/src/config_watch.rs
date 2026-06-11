//! Config-file watch (ADR-W020): hot-reload the impacted parts of the boot
//! config when the file changes on disk — through the SAME apply machinery the
//! Web/API uses — and change NOTHING when the new document is invalid.
//!
//! The watcher is a control-plane tokio task (never on the render thread):
//! it polls the config **path** (re-`stat` each tick, so a write-temp +
//! `rename(2)` lands as a normal change) and acts only on a fingerprint that
//! is **stable across two consecutive polls** (editors multi-write; the file
//! must settle first). On a debounced change it reparses + revalidates the
//! WHOLE document:
//!
//! * **invalid** ⇒ `tracing::warn!`, a latched `config-file-invalid` health
//!   warning on the engine's drop-oldest publisher (the UI banner + `GET
//!   /api/v1/health` surface), a `last_rejected` watch-status record — and
//!   **no state change anywhere** (the run keeps the last-good document);
//! * **valid** ⇒ a pure per-section [`ConfigDiff`] against the RUNNING
//!   baseline, applied through the one machinery: synthetic source
//!   adds/edits and any source removal ride `UpsertSource`/`RemoveSource` on
//!   the bounded command bus (ADR-W018), a layout/cells change rides the
//!   shared resolve+solve+Class-1-gate and `ApplyLayout` (ADR-W019), and
//!   every section without a live path reseeds its control store (the UI's
//!   truth) and latches a `config-file-requires-restart` warning naming it.
//!   The new file then becomes the baseline.
//!
//! The diff→apply core is the public [`apply_document_diff`] (ADR-W022 §2):
//! the watcher is ONE caller of it (audit actor `config-file`); the
//! `POST /api/v1/config/revert-to-start` route is another (audit actor the
//! requesting principal). There is exactly one apply implementation.
//!
//! This module lives in `multiview-control` so the revert route can reach it
//! (the dependency arrow is `cli → control`); `multiview_cli::config_watch`
//! re-exports it for the binary and the existing ADR-W020 integration tests.
//!
//! Isolation (invariants #1 + #10): every engine submission is the
//! non-blocking `try_submit`. A full bus sheds the submission — the watcher
//! then leaves the baseline AND the applied fingerprint **un-advanced**,
//! records a partial-apply rejection, raises the interim
//! `config-file-apply-incomplete` warning, and **re-applies the whole
//! (idempotent) change on a later poll** until every command lands; the
//! warning clears when the apply completes. Every publish is the drop-oldest
//! event broadcast, and every store touched is read-mostly control-plane
//! state. Nothing here can pace, stall, or back-pressure the output clock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_config::{ConfigDiff, MultiviewConfig, SourceChange};
use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};

use crate::command::{resolve_layout_document, Command, OperationId};
use crate::repository::LayoutInput;
use crate::resource_store::{ResourceInput, ResourceRepository};
use crate::state::AppState;
use crate::AuditAction;

/// The audit-log actor every file-applied change is recorded under.
const ACTOR: &str = "config-file";

/// How many consecutive missing-file polls before the watcher reports the
/// file as gone (a transient `ENOENT` mid-rename is normal and silent).
const MISSING_POLLS_BEFORE_REPORT: u32 = 5;

/// Watcher tuning. The default polls once per second — instant enough for a
/// hand edit, one `stat(2)` per second of cost, and no native watcher
/// dependency (ADR-W020 §1).
#[derive(Debug, Clone)]
pub struct WatchOptions {
    poll_interval: Duration,
    initial_observed: Option<String>,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            initial_observed: None,
        }
    }
}

impl WatchOptions {
    /// Override the poll interval (tests run the watcher at tens of
    /// milliseconds; production keeps the 1 s default).
    #[must_use]
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    /// Seed the loop's last-observed CONTENT with the boot-file text the run
    /// loaded at start (review m4): a settled observation whose content
    /// still equals the last observed content is adopted without applying —
    /// under `start = "resume"` the watcher's baseline is the RESUMED
    /// document while the file still holds the boot document, and the
    /// UNCHANGED file must never clobber the resumed Running state. An edit
    /// landing in the boot window IS a content change against this seed and
    /// still applies. Without a seed the first settled poll diffs whatever
    /// content it finds against the baseline (the ADR-W020 boot-window
    /// semantics, unchanged).
    #[must_use]
    pub fn with_initial_observed(mut self, content: String) -> Self {
        self.initial_observed = Some(content);
        self
    }
}

/// The most banked expected-write tokens kept at once (a stuck writer must
/// not grow memory; older banked tokens are shed oldest-first).
const MAX_EXPECTED_TOKENS: usize = 8;

/// One banked expected-write announcement: the content hash plus whether the
/// announced write actually LANDED on disk (review B1 (2)). A landed write
/// that a different settled content supersedes can never be the next settled
/// observation, so its token is drained; an unlanded announcement (the write
/// still in flight) survives an interleaving edit and suppresses exactly its
/// content when it finally lands (review m3, pinned).
#[derive(Debug, Clone, Copy)]
struct ExpectedToken {
    /// The announced content's hash.
    hash: u64,
    /// Whether the announcing writer confirmed the write landed on disk.
    landed: bool,
}

/// The cloneable handle [`spawn`] returns: self-write suppression for the
/// promote-to-boot lane ([`ConfigWatchHandle::expect_write`]) and a stop flag
/// for run teardown.
#[derive(Debug, Clone)]
pub struct ConfigWatchHandle {
    /// Banked expected (server-side) write announcements; each suppresses one
    /// reload of EXACTLY that content (review m3 — a banked token must never
    /// eat an unrelated external edit).
    expected: Arc<std::sync::Mutex<std::collections::VecDeque<ExpectedToken>>>,
    /// Raised by [`ConfigWatchHandle::stop`]; the loop exits on its next poll.
    stop: Arc<AtomicBool>,
}

impl ConfigWatchHandle {
    /// Mark one **expected** write of exactly `content`: when a debounced file
    /// change carries this content it is adopted as the new baseline WITHOUT
    /// applying anything (the server-side writer — e.g. a promote-to-boot
    /// flow — already applied the state it serialized). Call immediately
    /// before writing the file; confirm success with
    /// [`ConfigWatchHandle::mark_write_landed`] or release a failure with
    /// [`ConfigWatchHandle::release_write`]. The token is content-paired: an
    /// unrelated external edit landing first is still applied normally
    /// (review m3).
    pub fn expect_write(&self, content: &str) {
        let mut tokens = lock_tokens(&self.expected);
        if tokens.len() >= MAX_EXPECTED_TOKENS {
            tokens.pop_front();
        }
        tokens.push_back(ExpectedToken {
            hash: content_hash(content),
            landed: false,
        });
    }

    /// Confirm that the announced write of exactly `content` LANDED on disk
    /// (review B1 (2)). File writes are ordered: once a landed write is
    /// superseded by a different settled content it can never be the next
    /// settled observation, so the watcher drains its token — a stale token
    /// must never eat a much later REAL edit that restores the same bytes
    /// (`git checkout`, editor undo). An announcement never confirmed keeps
    /// the in-flight semantics (review m3, pinned): it survives interleaving
    /// edits and suppresses exactly its content when it finally settles.
    pub fn mark_write_landed(&self, content: &str) {
        let hash = content_hash(content);
        let mut tokens = lock_tokens(&self.expected);
        if let Some(token) = tokens.iter_mut().rev().find(|t| t.hash == hash) {
            token.landed = true;
        }
    }

    /// Release a banked expected-write token for exactly `content` (review
    /// B1 (3)): a server-side writer whose announced write FAILED must call
    /// this so the token cannot eat a later REAL external edit that happens
    /// to carry the same content. Returns whether a token was released.
    #[must_use = "whether a token was actually released is diagnostic; ignore it explicitly"]
    pub fn release_write(&self, content: &str) -> bool {
        let hash = content_hash(content);
        let mut tokens = lock_tokens(&self.expected);
        match tokens.iter().position(|t| t.hash == hash) {
            Some(index) => {
                tokens.remove(index);
                true
            }
            None => false,
        }
    }

    /// Stop the watcher (it exits on its next poll tick and marks the
    /// watch-status inactive).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Consume the banked token matching `text`'s content, if one exists —
    /// together with every OLDER banked token (review B1 (2)): writes are
    /// ordered, so announcements older than this settled write were
    /// superseded by it and must never eat a later real edit.
    fn consume_expected_for(&self, text: &str) -> bool {
        let hash = content_hash(text);
        let mut tokens = lock_tokens(&self.expected);
        match tokens.iter().position(|t| t.hash == hash) {
            Some(index) => {
                tokens.drain(..=index);
                true
            }
            None => false,
        }
    }

    /// Drain every LANDED token (review B1 (2)): called when a settled
    /// observation matches no banked announcement — an announcement whose
    /// write already landed was necessarily superseded by this settled
    /// content (writes are ordered), so its token is stale. Unlanded
    /// announcements (writes still in flight) are kept (review m3, pinned).
    fn drain_landed(&self) {
        let mut tokens = lock_tokens(&self.expected);
        tokens.retain(|t| !t.landed);
    }
}

/// Lock the token queue, recovering from a poisoned lock (a panicked banker
/// must not wedge the watcher).
fn lock_tokens(
    tokens: &std::sync::Mutex<std::collections::VecDeque<ExpectedToken>>,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<ExpectedToken>> {
    match tokens.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The ONE seam every server-side boot-file writer announces itself through
/// (ADR-W022 §6): call this immediately before atomically writing `content`
/// to the watched path, so the watcher adopts exactly that write as its new
/// baseline instead of re-applying it. The token is content-paired (review
/// m3): an unrelated external edit landing first is still applied normally.
/// Pair with [`confirm_server_write`] on success or
/// [`ConfigWatchHandle::release_write`] on failure.
pub fn expect_server_write(handle: &ConfigWatchHandle, content: &str) {
    handle.expect_write(content);
}

/// Confirm a previously announced server-side write of exactly `content`
/// landed on disk (review B1 (2)): call this immediately after the atomic
/// write succeeds, so a token whose write is later superseded by a different
/// settled content is drained instead of lingering to eat a future real edit
/// carrying the same bytes.
pub fn confirm_server_write(handle: &ConfigWatchHandle, content: &str) {
    handle.mark_write_landed(content);
}

/// A stable in-process hash of a write's content (`DefaultHasher` — this is a
/// self-match between our own writer and our own watcher, not an adversarial
/// boundary).
fn content_hash(text: &str) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Spawn the config-file watcher over `path` on the control-plane tokio
/// runtime. `baseline` is the document the run booted with (the currently
/// RUNNING state — under an ADR-W022 `start = "resume"` this is the resumed
/// document, while `path` stays the BOOT file); `state` is the router's
/// `AppState` (the one set of stores, the bounded command bus, the
/// drop-oldest publisher, the audit log, and the watch-status slot).
///
/// The returned handle is also installed into `state`'s shared watch-handle
/// slot, so the promote-to-boot route can reach the [`expect_server_write`]
/// suppression seam of the SAME watcher this run serves.
pub fn spawn(
    path: PathBuf,
    baseline: MultiviewConfig,
    state: AppState,
    options: WatchOptions,
) -> ConfigWatchHandle {
    let handle = ConfigWatchHandle {
        expected: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        stop: Arc::new(AtomicBool::new(false)),
    };
    state.install_watch_handle(handle.clone());
    state.config_watch.mark_active(&path.display().to_string());
    tracing::info!(
        path = %path.display(),
        poll = ?options.poll_interval,
        "watching the config file for external changes (ADR-W020)"
    );
    // The watcher starts with NO applied fingerprint (review M2): the first
    // SETTLED poll re-reads the file and diffs it against the boot baseline,
    // so an edit landing between the boot-time `load_validated` and this
    // spawn is APPLIED — never silently adopted. An unchanged file settles to
    // an empty diff and is adopted with no commands and no warnings.
    let task_handle = handle.clone();
    tokio::spawn(watch_loop(path, baseline, state, options, task_handle));
    handle
}

/// Marks the watch-status inactive when the watcher task ends — via
/// [`ConfigWatchHandle::stop`], run teardown, or an unexpected panic
/// unwinding the task (review m2): the UI must never show "watching" for a
/// dead watcher.
struct ActiveGuard(Arc<crate::watch_status::ConfigWatchStatus>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.mark_inactive();
    }
}

/// A cheap content-change fingerprint of the watched path: length + mtime +
/// inode. Re-`stat`ing the PATH (never a held fd) makes a write-temp +
/// `rename(2)` land as a normal change (new inode).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Fingerprint {
    len: u64,
    modified: Option<std::time::SystemTime>,
    inode: u64,
}

/// Probe the path's fingerprint; [`None`] when the file is (transiently)
/// missing. `tokio::fs` so the `stat(2)` rides the blocking pool, never the
/// control-plane reactor (review m6).
async fn probe(path: &Path) -> Option<Fingerprint> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt as _;
        meta.ino()
    };
    #[cfg(not(unix))]
    let inode = 0_u64;
    Some(Fingerprint {
        len: meta.len(),
        modified: meta.modified().ok(),
        inode,
    })
}

/// The poll loop: debounce (a new fingerprint must be observed on two
/// consecutive polls), suppress expected writes, and hand a settled change to
/// [`apply_change`].
#[allow(clippy::too_many_lines)]
// reason: the poll loop is one linear settle/adopt/apply state machine whose
// arms share six pieces of latched state (applied/rejected/candidate
// fingerprints, last-observed content, the two warning latches — the review
// M1/M2/m4/m5 interplay); splitting it further would hand most of that state
// as `&mut` bundles to helpers. Everything separable is already factored out
// (`resolve_latched_on_unchanged`, `adopt_expected_text`, `apply_change`).
async fn watch_loop(
    path: PathBuf,
    mut baseline: MultiviewConfig,
    state: AppState,
    options: WatchOptions,
    handle: ConfigWatchHandle,
) {
    // Inactive-on-drop: stop, teardown, and panic all flip the status
    // (review m2).
    let _active = ActiveGuard(Arc::clone(&state.config_watch));
    // The fingerprint of the content `baseline` reflects. `None` at start
    // (review M2): the first settled poll diffs the file against the boot
    // baseline, closing the boot-load → watcher-spawn window.
    let mut applied: Option<Fingerprint> = None;
    // The last CONTENT this loop settled on (review m4) — seeded with the
    // boot-load text when the caller provides it, advanced on every settled
    // apply/adopt (never on reject/retry). A settled fingerprint change whose
    // content still equals it is adopted without applying: under a resume
    // the unchanged boot file must never clobber the resumed baseline.
    let mut last_observed: Option<String> = options.initial_observed.clone();
    // The fingerprint of a latched-REJECTED content (invalid / unreadable /
    // a buggy expected write): handled once, never re-warned each poll —
    // and, unlike `applied`, never treated as "the file matches the running
    // configuration".
    let mut rejected: Option<Fingerprint> = None;
    let mut candidate: Option<Fingerprint> = None;
    let mut missing_polls: u32 = 0;
    let mut missing_reported = false;
    // Whether a `config-file-invalid` warning is currently raised (cleared on
    // the next valid apply, or when the file is back at the applied content).
    let mut invalid_active = false;
    // Whether a `config-file-apply-incomplete` warning is currently raised
    // (review M1: engine command(s) shed on a full bus; cleared when the
    // retried apply completes).
    let mut incomplete_active = false;
    loop {
        tokio::time::sleep(options.poll_interval).await;
        if handle.stop.load(Ordering::Acquire) {
            tracing::debug!(path = %path.display(), "config-file watcher stopped");
            return;
        }
        let Some(now) = probe(&path).await else {
            // Mid-rename ENOENT is normal; a file that STAYS missing is
            // reported once (the running configuration is unchanged).
            candidate = None;
            missing_polls = missing_polls.saturating_add(1);
            if missing_polls >= MISSING_POLLS_BEFORE_REPORT && !missing_reported {
                missing_reported = true;
                reject(
                    &state,
                    &path,
                    "the watched config file is missing",
                    &mut invalid_active,
                );
            }
            continue;
        };
        missing_polls = 0;
        missing_reported = false;
        if applied.as_ref() == Some(&now) {
            candidate = None;
            // The file is present at the already-applied content, so any
            // latched condition has resolved without new content to apply
            // (review m5: e.g. the file was renamed away — warned missing —
            // and renamed back).
            resolve_latched_on_unchanged(
                &state,
                &path,
                &baseline,
                &mut invalid_active,
                &mut incomplete_active,
            );
            continue;
        }
        if rejected.as_ref() == Some(&now) {
            // Already rejected and warned exactly once; latched until the
            // content changes again.
            candidate = None;
            continue;
        }
        if candidate.as_ref() != Some(&now) {
            // First sighting of this fingerprint: wait one more poll for the
            // writer to settle (editors multi-write).
            candidate = Some(now);
            continue;
        }
        // Stable across two polls: act on it. Read the content once — the
        // expected-write check is content-paired (review m3). `tokio::fs` so
        // the read rides the blocking pool (review m6).
        candidate = None;
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => {
                rejected = Some(now);
                reject(
                    &state,
                    &path,
                    &format!("the file cannot be read: {error}"),
                    &mut invalid_active,
                );
                continue;
            }
        };
        if handle.consume_expected_for(&text) {
            if adopt_expected_text(&path, &text, &mut baseline) {
                applied = Some(now);
                rejected = None;
                last_observed = Some(text);
            } else {
                rejected = Some(now);
            }
            continue;
        }
        // This settled content matches no banked announcement: any
        // announcement whose write already LANDED was superseded by it
        // (writes are ordered) — drain those tokens so a stale one can never
        // eat a later real edit carrying the same bytes (review B1 (2)).
        // Unlanded announcements (writes still in flight) survive
        // (review m3, pinned).
        handle.drain_landed();
        if last_observed.as_deref() == Some(text.as_str()) {
            // Review m4: the CONTENT is unchanged — a touch/identical rewrite
            // or, under `start = "resume"`, the unchanged boot file observed
            // for the first time. Nothing to apply; the latched conditions
            // resolve exactly as in the fingerprint-match branch above.
            applied = Some(now);
            rejected = None;
            resolve_latched_on_unchanged(
                &state,
                &path,
                &baseline,
                &mut invalid_active,
                &mut incomplete_active,
            );
            continue;
        }
        match apply_change(
            &path,
            &text,
            &mut baseline,
            &state,
            &mut invalid_active,
            &mut incomplete_active,
        ) {
            ApplyResult::Settled => {
                applied = Some(now);
                rejected = None;
                last_observed = Some(text);
            }
            ApplyResult::RejectedInvalid => {
                rejected = Some(now);
            }
            ApplyResult::Retry => {
                // Review M1: engine command(s) were shed — leave `applied`,
                // the baseline AND `last_observed` un-advanced so a later
                // poll re-reads and re-applies the whole (idempotent) change.
            }
        }
    }
}

/// Resolve the latched warning conditions when the loop settles on content
/// it has already handled — the fingerprint-match arm (review m5: the file
/// was renamed away and back) and the content-match arm (review m4: a
/// touch/identical rewrite, or the unchanged boot file under a resume).
/// A latched `config-file-invalid` clears, and a pending partial (shed)
/// apply is abandoned: nothing is pending for the engine any more, but the
/// stores may have followed the abandoned content — re-converge them to the
/// running `baseline` before clearing the interim warning.
fn resolve_latched_on_unchanged(
    state: &AppState,
    path: &Path,
    baseline: &MultiviewConfig,
    invalid_active: &mut bool,
    incomplete_active: &mut bool,
) {
    if *invalid_active {
        clear_invalid(
            state,
            path,
            "the file is back at the already-applied content",
            invalid_active,
        );
    }
    if *incomplete_active {
        resync_all_stores(state, ACTOR, baseline);
        clear_apply_incomplete(state, path, incomplete_active);
    }
}

/// Adopt an **expected** (server-side) write as the new baseline without
/// applying anything — the writer already applied the state it serialized. A
/// write that does not parse/validate is still warned (a buggy writer must
/// never be silent), and the baseline is kept. Returns whether the write was
/// adopted.
fn adopt_expected_text(path: &Path, text: &str, baseline: &mut MultiviewConfig) -> bool {
    match parse_validated(text) {
        Ok(next) => {
            tracing::debug!(
                path = %path.display(),
                "expected (self) config write adopted as the new baseline; no reload"
            );
            *baseline = next;
            true
        }
        Err(reason) => {
            tracing::warn!(
                path = %path.display(),
                reason = %reason,
                "an EXPECTED config write does not validate — the writer is buggy; \
                 keeping the previous baseline"
            );
            false
        }
    }
}

/// Parse + validate the whole document text, with a human-readable reason on
/// any failure.
fn parse_validated(text: &str) -> Result<MultiviewConfig, String> {
    let next = MultiviewConfig::load_from_toml(text)
        .map_err(|e| format!("the document does not parse: {e}"))?;
    next.validate()
        .map_err(|e| format!("the document does not validate: {e}"))?;
    Ok(next)
}

/// How the loop must advance its fingerprints after [`apply_change`].
enum ApplyResult {
    /// Fully handled — applied (or identical content adopted); the content's
    /// fingerprint becomes the `applied` baseline fingerprint.
    Settled,
    /// The document was invalid: warned + latched once; the fingerprint is
    /// remembered as rejected (never as applied).
    RejectedInvalid,
    /// Only PARTIALLY applied — engine command(s) shed on a full bus (review
    /// M1). Neither the baseline nor any fingerprint advanced: a later poll
    /// re-reads and re-applies the whole (idempotent) change.
    Retry,
}

/// Handle one settled external change: whole-document validate, per-section
/// diff, apply through the one machinery, advance the baseline — unless a
/// command was shed, in which case NOTHING advances and the apply is retried
/// on a later poll (review M1).
fn apply_change(
    path: &Path,
    text: &str,
    baseline: &mut MultiviewConfig,
    state: &AppState,
    invalid_active: &mut bool,
    incomplete_active: &mut bool,
) -> ApplyResult {
    let next = match parse_validated(text) {
        Ok(next) => next,
        Err(reason) => {
            reject(state, path, &reason, invalid_active);
            return ApplyResult::RejectedInvalid;
        }
    };
    let diff = ConfigDiff::between(baseline, &next);
    if diff.is_empty() {
        // A touch / rewrite with identical content: adopt silently. If a
        // partial (shed) apply was pending, the file was REVERTED to the
        // running baseline: nothing is pending for the engine any more, but
        // the stores may have followed the abandoned content — re-converge
        // them to the file before clearing the interim warning.
        if *incomplete_active {
            resync_all_stores(state, ACTOR, &next);
            clear_apply_incomplete(state, path, incomplete_active);
        }
        *baseline = next;
        clear_invalid(
            state,
            path,
            "a subsequent valid write applied",
            invalid_active,
        );
        tracing::debug!(path = %path.display(), "config file rewritten with identical content");
        return ApplyResult::Settled;
    }
    let outcome = apply_document_diff(state, ACTOR, &diff, &next);
    // The document is valid either way — the invalid latch clears even when
    // the apply is incomplete (the conditions are independent).
    clear_invalid(
        state,
        path,
        "a subsequent valid write applied",
        invalid_active,
    );
    if outcome.shed > 0 {
        // Review M1: NEVER claim "applied" while a command was shed. Leave
        // the baseline un-advanced, record the partial apply as a rejection,
        // raise the interim warning, and let the next poll retry the whole
        // (idempotent) apply. Restart-pending accounting also waits for the
        // completed retry, so the status never gets ahead of the engine.
        state.config_watch.record_rejected(
            now_ms(state),
            &format!(
                "partially applied: {} engine command(s) shed on a full command bus; \
                 the watcher retries the whole change on its next poll",
                outcome.shed
            ),
        );
        publish_apply_incomplete(state, path, outcome.shed, incomplete_active);
        return ApplyResult::Retry;
    }
    *baseline = next;
    clear_apply_incomplete(state, path, incomplete_active);
    let summary = outcome.summary();
    state.config_watch.record_applied(now_ms(state), &summary);
    if !outcome.restart.is_empty() {
        state
            .config_watch
            .add_restart_pending(outcome.restart.iter().cloned());
        // Warn with the LATCHED union (every section pending since start), so
        // the single coalesced warning entry stays complete.
        let pending = state.config_watch.snapshot().restart_pending;
        publish_requires_restart(state, &pending);
    }
    tracing::info!(
        path = %path.display(),
        summary = %summary,
        "applied an external config-file change (ADR-W020)"
    );
    ApplyResult::Settled
}

/// What one document-diff apply did: the per-section human summary parts, the
/// restart-only sections, and how many engine commands were shed on a full
/// bus (review M1: any shed means the apply is incomplete — the watcher
/// retries it on a later poll; the revert route reports it as a partial
/// revert).
#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    /// Per-section human-readable summary parts (e.g. `sources: in_a changed`),
    /// in apply order.
    pub parts: Vec<String>,
    /// The sections that could not hot-apply and take effect on restart.
    pub restart: BTreeSet<String>,
    /// How many engine commands were shed on a full bus (0 = fully applied).
    pub shed: u32,
}

impl ApplyOutcome {
    /// The one-line summary (the watch-status / log form of [`parts`](Self::parts)).
    #[must_use]
    pub fn summary(&self) -> String {
        self.parts.join("; ")
    }
}

/// Apply a validated per-section diff toward `next` through the one apply
/// machinery (ADR-W020/W022): engine commands for the live-appliable parts,
/// store reseeds for everything the UI mirrors, restart-section accounting for
/// the rest. Every store mutation is audited under `actor` — `config-file`
/// for the watcher, the requesting principal for the revert-to-start route.
pub fn apply_document_diff(
    state: &AppState,
    actor: &str,
    diff: &ConfigDiff,
    next: &MultiviewConfig,
) -> ApplyOutcome {
    let mut restart: BTreeSet<String> = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();
    let mut shed: u32 = 0;

    // 1. Sources FIRST (FIFO bus): a layout rebinding to a just-added source
    //    must find its store registered in the same frame-boundary pass.
    if !diff.sources.is_empty() {
        parts.push(apply_source_changes(state, diff, &mut restart, &mut shed));
        resync_store(state, actor, &state.sources, &desired_sources(next));
    }

    // 2. Canvas: the pinned signal is Class-2 (ADR-R004) and the cosmetic
    //    axes have no live render path either — restart, never silently.
    if diff.canvas_signal_changed || diff.canvas_cosmetic_changed {
        restart.insert("canvas".to_owned());
        if diff.canvas_signal_changed {
            tracing::warn!(
                "config file changed the canvas geometry/cadence — a Class-2 change \
                 (the output canvas is pinned for the session, ADR-R004); it applies on restart"
            );
        } else {
            tracing::warn!(
                "config file changed canvas pixel-format/background/colour; it applies on restart"
            );
        }
        parts.push("canvas changed".to_owned());
    }

    // 3. Layout/cells: the SAME resolve+solve+Class-1-gate as the
    //    apply-layout route, then the same ApplyLayout command (ADR-W019).
    if diff.layout_changed {
        parts.push(apply_layout_change(
            state,
            actor,
            diff,
            next,
            &mut restart,
            &mut shed,
        ));
    }

    // 4. Every other changed section: reseed its store where one exists (the
    //    UI's truth follows the file) + restart accounting (no live path yet).
    for section in &diff.changed_sections {
        restart.insert((*section).to_owned());
        match *section {
            "outputs" => resync_store(state, actor, &state.outputs, &desired_outputs(next)),
            "overlays" => resync_store(state, actor, &state.overlays, &desired_overlays(next)),
            "probes" => resync_store(state, actor, &state.probes, &desired_probes(next)),
            "devices" => {
                resync_store(state, actor, &state.devices, &desired_devices(next));
                for device in &next.devices {
                    state.device_status.ensure(&device.id);
                }
            }
            "sync_groups" => {
                resync_store(state, actor, &state.sync_groups, &desired_sync_groups(next));
            }
            "audio" => resync_audio(state, actor, next),
            // No store is boot-seeded for these; the file itself is the
            // durable truth and the restart warning names them.
            _ => {}
        }
        parts.push(format!("{section} changed"));
    }

    if !restart.is_empty() {
        let names: Vec<&str> = restart.iter().map(String::as_str).collect();
        parts.push(format!("restart pending: {}", names.join(", ")));
    }
    ApplyOutcome {
        parts,
        restart,
        shed,
    }
}

/// Apply the source diff exactly as the sources routes do (ADR-W018):
/// synthetic add/edit ⇒ live `UpsertSource`; a synthetic→decoded kind change
/// ⇒ live `RemoveSource` (stop the stale generator) + restart for the new
/// document; decoded add/edit ⇒ restart, honestly; any removal ⇒ live
/// `RemoveSource` (bound tiles ride their `on_loss` slate).
fn apply_source_changes(
    state: &AppState,
    diff: &ConfigDiff,
    restart: &mut BTreeSet<String>,
    shed: &mut u32,
) -> String {
    let mut described: Vec<String> = Vec::new();
    for change in &diff.sources {
        match change {
            SourceChange::Added(source) => {
                described.push(format!("{} added", source.id));
                if source.kind.is_synthetic() {
                    if !submit(
                        state,
                        Command::UpsertSource {
                            op: OperationId::new(),
                            source: source.clone(),
                        },
                    ) {
                        *shed = shed.saturating_add(1);
                    }
                } else {
                    restart.insert("sources".to_owned());
                    tracing::warn!(
                        source = %source.id,
                        "config file added a non-synthetic source; it applies on restart \
                         (consistent with the API's restart semantics)"
                    );
                }
            }
            SourceChange::Changed { previous, next } => {
                described.push(format!("{} changed", next.id));
                if next.kind.is_synthetic() {
                    if !submit(
                        state,
                        Command::UpsertSource {
                            op: OperationId::new(),
                            source: next.clone(),
                        },
                    ) {
                        *shed = shed.saturating_add(1);
                    }
                } else {
                    if previous.kind.is_synthetic() {
                        // Mirror the sources route: stop the stale generator
                        // now; a frozen synthetic pretending to be the new
                        // URL would be dishonest.
                        if !submit(
                            state,
                            Command::RemoveSource {
                                op: OperationId::new(),
                                id: next.id.clone(),
                            },
                        ) {
                            *shed = shed.saturating_add(1);
                        }
                    }
                    restart.insert("sources".to_owned());
                    tracing::warn!(
                        source = %next.id,
                        "config file changed a source to a non-synthetic kind; it applies on restart"
                    );
                }
            }
            SourceChange::Removed(id) => {
                described.push(format!("{id} removed"));
                if !submit(
                    state,
                    Command::RemoveSource {
                        op: OperationId::new(),
                        id: id.clone(),
                    },
                ) {
                    *shed = shed.saturating_add(1);
                }
            }
        }
    }
    format!("sources: {}", described.join(", "))
}

/// Apply a layout/cells change through the shared route machinery
/// ([`resolve_layout_document`] → `ApplyLayout`), and reseed the working
/// layout in the layouts repository so the UI editor follows the file.
fn apply_layout_change(
    state: &AppState,
    actor: &str,
    diff: &ConfigDiff,
    next: &MultiviewConfig,
    restart: &mut BTreeSet<String>,
    shed: &mut u32,
) -> String {
    let id = state
        .working_layout_id
        .clone()
        .unwrap_or_else(|| "working".to_owned());
    let Some(body) = working_layout_body(next) else {
        // Serialising plain derived config types cannot fail in practice;
        // surfaced honestly rather than panicking on the watcher task.
        restart.insert("layout".to_owned());
        return "layout changed (could not serialize; applies on restart)".to_owned();
    };
    reseed_working_layout(state, actor, &id, &body);
    if diff.canvas_signal_changed {
        // The Class-2 canvas change already rides the restart surface; a live
        // apply against the pinned canvas would be refused anyway.
        restart.insert("layout".to_owned());
        tracing::warn!(
            "config file changed the layout alongside a Class-2 canvas change; \
             both apply on restart"
        );
        return "layout changed (Class-2 canvas; applies on restart)".to_owned();
    }
    match resolve_layout_document(&id, &body, state.running_canvas.as_ref()) {
        Ok(resolved) => {
            // Review M1: claim "applied live" ONLY when the command actually
            // landed on the bus; a shed leaves the apply incomplete and the
            // watcher retries it on a later poll.
            if submit(
                state,
                Command::ApplyLayout {
                    op: OperationId::new(),
                    layout: id,
                    document: Some(Box::new(resolved)),
                },
            ) {
                "layout applied live".to_owned()
            } else {
                *shed = shed.saturating_add(1);
                "layout apply shed (retried on the next poll)".to_owned()
            }
        }
        Err(error) => {
            // The document validated as a whole, so this is the Class-1 gate
            // (or an unseeded running canvas failing closed) — restart-only.
            restart.insert("layout".to_owned());
            tracing::warn!(
                error = %error,
                "config file layout change cannot apply live; it applies on restart"
            );
            "layout changed (held; applies on restart)".to_owned()
        }
    }
}

/// The authored `{canvas, layout, cells}` working-layout body (the exact
/// shape `seed_resources` seeds and the apply-layout route resolves).
fn working_layout_body(config: &MultiviewConfig) -> Option<serde_json::Value> {
    let canvas = to_body(&config.canvas)?;
    let layout = to_body(&config.layout)?;
    let cells = to_body(&config.cells)?;
    Some(serde_json::json!({
        "canvas": canvas,
        "layout": layout,
        "cells": cells,
    }))
}

/// Update (or recreate) the working layout in the layouts repository so the
/// UI layout editor reflects the file immediately.
fn reseed_working_layout(state: &AppState, actor: &str, id: &str, body: &serde_json::Value) {
    let input = LayoutInput {
        name: id.to_owned(),
        body: body.clone(),
    };
    let updated = state
        .repository
        .update_layout(id, input.clone())
        .or_else(|_| state.repository.create_layout(id, input));
    match updated {
        Ok(_) => {
            state.audit(
                actor,
                AuditAction::Update,
                crate::repository::LAYOUT_KIND,
                id,
                Some(body.clone()),
            );
        }
        Err(error) => {
            tracing::warn!(
                layout = %id,
                error = %error,
                "could not reseed the working layout from the config file"
            );
        }
    }
}

/// Bring a resource store's contents in line with the document's desired
/// state: create/update/delete by id, audit-logged under `actor`. A store
/// fault is warned and skipped — a flaky store must never wedge the caller.
fn resync_store(
    state: &AppState,
    actor: &str,
    store: &Arc<dyn ResourceRepository>,
    desired: &[(String, ResourceInput)],
) {
    let existing: Vec<String> = match store.list() {
        Ok(list) => list.into_iter().map(|v| v.resource.id).collect(),
        Err(error) => {
            tracing::warn!(kind = store.kind(), error = %error, "config resync: list failed");
            return;
        }
    };
    for (id, input) in desired {
        let result = if existing.iter().any(|e| e == id) {
            store.update(id, input.clone()).map(|_| AuditAction::Update)
        } else {
            store.create(id, input.clone()).map(|_| AuditAction::Create)
        };
        match result {
            Ok(action) => state.audit(actor, action, store.kind(), id, Some(input.body.clone())),
            Err(error) => {
                tracing::warn!(kind = store.kind(), id = %id, error = %error, "config resync: write failed");
            }
        }
    }
    for id in existing {
        if !desired.iter().any(|(want, _)| want == &id) {
            match store.delete(&id) {
                Ok(()) => state.audit(actor, AuditAction::Delete, store.kind(), &id, None),
                Err(error) => {
                    tracing::warn!(kind = store.kind(), id = %id, error = %error, "config resync: delete failed");
                }
            }
        }
    }
}

/// Replace the audio-routing singleton from the document's `[audio]` block (a
/// bounded CAS loop over the versioned store). A REMOVED block cannot be
/// expressed on the singleton store today; the restart warning covers it.
fn resync_audio(state: &AppState, actor: &str, next: &MultiviewConfig) {
    let Some(routing) = next.audio.clone() else {
        tracing::warn!(
            "config document removed the [audio] block; the audio-routing store keeps its last \
             value until restart"
        );
        return;
    };
    let mut version = state.audio_routing.version();
    for _ in 0_u8..4 {
        match state.audio_routing.replace_if(version, routing.clone()) {
            Ok(_) => {
                state.audit(
                    actor,
                    AuditAction::Update,
                    crate::AUDIO_ROUTING_KIND,
                    crate::AUDIO_ROUTING_ID,
                    to_body(&routing),
                );
                return;
            }
            Err(actual) => version = actual,
        }
    }
    tracing::warn!("config resync: the audio-routing store kept changing; giving up");
}

/// The sources store's desired contents for `config` (mirrors `seed_resources`).
fn desired_sources(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .sources
        .iter()
        .filter_map(|source| {
            Some((
                source.id.clone(),
                ResourceInput {
                    name: source
                        .display_name
                        .clone()
                        .unwrap_or_else(|| source.id.clone()),
                    body: to_body(source)?,
                },
            ))
        })
        .collect()
}

/// The outputs store's desired contents: stable index-derived `output-{n}`
/// ids in config order, named by the `kind` tag (mirrors `seed_resources`).
fn desired_outputs(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .outputs
        .iter()
        .enumerate()
        .filter_map(|(index, output)| {
            let body = to_body(output)?;
            let kind = body
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("output")
                .to_owned();
            Some((
                format!("output-{index}"),
                ResourceInput { name: kind, body },
            ))
        })
        .collect()
}

/// The overlays store's desired contents (id-named, mirrors `seed_resources`).
fn desired_overlays(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .overlays
        .iter()
        .filter_map(|overlay| {
            Some((
                overlay.id.clone(),
                ResourceInput {
                    name: overlay.id.clone(),
                    body: to_body(overlay)?,
                },
            ))
        })
        .collect()
}

/// The probes store's desired contents (id-named, mirrors `seed_resources`).
fn desired_probes(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .probes
        .iter()
        .filter_map(|probe| {
            Some((
                probe.id.clone(),
                ResourceInput {
                    name: probe.id.clone(),
                    body: to_body(probe)?,
                },
            ))
        })
        .collect()
}

/// The devices store's desired contents (mirrors `seed_resources`).
fn desired_devices(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .devices
        .iter()
        .filter_map(|device| {
            Some((
                device.id.clone(),
                ResourceInput {
                    name: device
                        .display_name
                        .clone()
                        .unwrap_or_else(|| device.id.clone()),
                    body: to_body(device)?,
                },
            ))
        })
        .collect()
}

/// The sync-groups store's desired contents (mirrors `seed_resources`).
fn desired_sync_groups(config: &MultiviewConfig) -> Vec<(String, ResourceInput)> {
    config
        .sync_groups
        .iter()
        .filter_map(|group| {
            Some((
                group.id.clone(),
                ResourceInput {
                    name: group.id.clone(),
                    body: to_body(group)?,
                },
            ))
        })
        .collect()
}

/// Serialize a config value to its canonical JSON body, warning (never
/// panicking) on the in-practice-impossible failure.
fn to_body(value: &impl serde::Serialize) -> Option<serde_json::Value> {
    match serde_json::to_value(value) {
        Ok(body) => Some(body),
        Err(error) => {
            tracing::warn!(error = %error, "config resync: serializing a config value failed");
            None
        }
    }
}

/// Submit a command on the bounded, non-blocking bus (invariant #10). Returns
/// whether it landed; a full/closed bus sheds with a warning — the caller
/// counts the shed and the watcher retries the whole apply on a later poll
/// (review M1).
fn submit(state: &AppState, command: Command) -> bool {
    let kind = command.kind();
    match state.commands.try_submit(command) {
        Ok(_op) => true,
        Err(error) => {
            tracing::warn!(
                command = kind,
                error = %error,
                "config-file apply: the engine command bus shed this change; \
                 the watcher retries the whole apply on its next poll"
            );
            false
        }
    }
}

/// Record + surface an INVALID file write: warn, watch-status `last_rejected`,
/// and the latched `config-file-invalid` health warning — and change nothing.
fn reject(state: &AppState, path: &Path, reason: &str, invalid_active: &mut bool) {
    tracing::warn!(
        path = %path.display(),
        reason = %reason,
        "ignoring an INVALID config-file write; the running configuration is unchanged"
    );
    state.config_watch.record_rejected(now_ms(state), reason);
    *invalid_active = true;
    state
        .engine
        .publish_event(Event::HealthWarningRaised(invalid_warning(
            path,
            reason,
            now_nanos(state),
            true,
        )));
}

/// Clear a previously-raised `config-file-invalid` warning: the invalid
/// condition resolved, either because a subsequent valid write applied or
/// because the file is back at the already-applied content (review m5).
fn clear_invalid(state: &AppState, path: &Path, reason: &str, invalid_active: &mut bool) {
    if !*invalid_active {
        return;
    }
    *invalid_active = false;
    state
        .engine
        .publish_event(Event::HealthWarningCleared(invalid_warning(
            path,
            reason,
            now_nanos(state),
            false,
        )));
}

/// Raise (or refresh) the interim `config-file-apply-incomplete` warning: a
/// valid change was only PARTIALLY applied because `shed` engine command(s)
/// were shed on a full bus (review M1). The watcher retries the whole apply
/// on its next poll and clears this when it completes.
fn publish_apply_incomplete(
    state: &AppState,
    path: &Path,
    shed: u32,
    incomplete_active: &mut bool,
) {
    *incomplete_active = true;
    state
        .engine
        .publish_event(Event::HealthWarningRaised(apply_incomplete_warning(
            path,
            shed,
            now_nanos(state),
            true,
        )));
}

/// Clear a previously-raised `config-file-apply-incomplete` warning: the
/// retried apply completed (or the file reverted to the running baseline, so
/// nothing is pending for the engine any more).
fn clear_apply_incomplete(state: &AppState, path: &Path, incomplete_active: &mut bool) {
    if !*incomplete_active {
        return;
    }
    *incomplete_active = false;
    state
        .engine
        .publish_event(Event::HealthWarningCleared(apply_incomplete_warning(
            path,
            0,
            now_nanos(state),
            false,
        )));
}

/// Build the `config-file-apply-incomplete` warning (raise and clear share
/// the shape; the store coalesces on the code).
fn apply_incomplete_warning(path: &Path, shed: u32, since: i64, active: bool) -> HealthWarning {
    HealthWarning {
        code: WarningCode::ConfigFileApplyIncomplete,
        severity: WarningSeverity::Warning,
        subsystem: "config".to_owned(),
        message: format!(
            "A valid change to the config file {} is only PARTIALLY applied: {shed} engine \
             command(s) were shed on a full command bus; the watcher retries the whole change \
             on its next poll.",
            path.display()
        ),
        remediation: "No action needed — the watcher retries automatically and clears this \
                      warning when the apply completes; investigate a persistently full \
                      command bus if it does not."
            .to_owned(),
        since,
        active,
    }
}

/// Re-converge every store-backed section (and the working layout) to
/// `config`. Two callers: the watcher, when a partial (shed) apply is
/// abandoned by a file revert (the stores followed the FILE on the first
/// attempt — ADR-W020 §5 — and must come back in line with the document that
/// IS the running truth); and the revert-to-start route, which rolls the
/// stores back to the pre-revert Running document when its engine commands
/// shed (a shed revert applies nothing durable, so the retry re-runs the
/// whole revert — ADR-W022 §5). Idempotent.
pub(crate) fn resync_all_stores(state: &AppState, actor: &str, config: &MultiviewConfig) {
    resync_store(state, actor, &state.sources, &desired_sources(config));
    resync_store(state, actor, &state.outputs, &desired_outputs(config));
    resync_store(state, actor, &state.overlays, &desired_overlays(config));
    resync_store(state, actor, &state.probes, &desired_probes(config));
    resync_store(state, actor, &state.devices, &desired_devices(config));
    for device in &config.devices {
        state.device_status.ensure(&device.id);
    }
    resync_store(
        state,
        actor,
        &state.sync_groups,
        &desired_sync_groups(config),
    );
    if config.audio.is_some() {
        resync_audio(state, actor, config);
    }
    let id = state
        .working_layout_id
        .clone()
        .unwrap_or_else(|| "working".to_owned());
    if let Some(body) = working_layout_body(config) {
        reseed_working_layout(state, actor, &id, &body);
    }
}

/// Build the `config-file-invalid` warning (raise and clear share the shape;
/// the store coalesces on the code).
fn invalid_warning(path: &Path, reason: &str, since: i64, active: bool) -> HealthWarning {
    HealthWarning {
        code: WarningCode::ConfigFileInvalid,
        severity: WarningSeverity::Warning,
        subsystem: "config".to_owned(),
        message: format!(
            "The config file {} changed on disk but the new document is invalid ({reason}); \
             NOTHING was applied — the run keeps the last-good configuration.",
            path.display()
        ),
        remediation: "Fix the file (run `multiview validate` against it); the next valid \
                      write applies and clears this warning."
            .to_owned(),
        since,
        active,
    }
}

/// Raise (or refresh) the latched `config-file-requires-restart` warning
/// naming every pending section.
fn publish_requires_restart(state: &AppState, pending: &[String]) {
    let sections = pending.join(", ");
    tracing::warn!(
        sections = %sections,
        "config file changed sections that only apply on RESTART; the running process \
         may differ from the file until then"
    );
    // Review m7: the warning is LATCHED (a later revert cannot un-ring the
    // bell for state the engine never adopted), so the message says "changed
    // since boot" / "may differ" — the file's CURRENT content might have been
    // reverted since the change was seen.
    state
        .engine
        .publish_event(Event::HealthWarningRaised(HealthWarning {
            code: WarningCode::ConfigFileRequiresRestart,
            severity: WarningSeverity::Warning,
            subsystem: "config".to_owned(),
            message: format!(
                "Section(s) [{sections}] of the config file changed since boot in ways that \
                 cannot hot-apply; the running process may differ from the file until a restart."
            ),
            remediation: "Restart multiview to apply these sections (live-appliable changes \
                          were already applied)."
                .to_owned(),
            since: now_nanos(state),
            active: true,
        }));
}

/// The control plane's clock as Unix nanoseconds (the same `AckClock` the
/// audit log stamps with — injectable in tests).
fn now_nanos(state: &AppState) -> i64 {
    state.ack_now().as_nanos()
}

/// The control plane's clock as Unix milliseconds.
fn now_ms(state: &AppState) -> i64 {
    now_nanos(state).div_euclid(1_000_000)
}
