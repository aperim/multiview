//! The Boot / Loaded / Running configuration model (ADR-W022).
//!
//! * **Boot** is the config file `multiview run` started with — the watched,
//!   hand-edited cold-start baseline.
//! * **Loaded** is the immutable snapshot of Boot taken at process start,
//!   held in memory ([`BootModel::loaded`]) and persisted to
//!   `<config-dir>/.multiview/loaded.toml` ([`persist_loaded`]).
//! * **Running** is Loaded + every live change since. It is continuously
//!   persisted — debounced, atomic-rename, machine-written, NEVER watched —
//!   to `<config-dir>/.multiview/active.toml`, composed via the SAME document
//!   composition `GET /api/v1/config/export` uses ([`persist_running_now`]).
//!   The trigger is the ONE audit choke point: every successful mutation
//!   passes [`crate::AppState::audit`], which fires the coalescing
//!   `running_changed` notify the [`spawn_running_persist`] task waits on.
//!
//! Cold start: `[control] start = "resume"` loads a valid `active.toml` as
//! the starting Running state ([`load_resume_config`]); invalid/missing falls
//! back to boot with a warning (the caller surfaces the reason). Loaded stays
//! the boot snapshot in both modes.
//!
//! Isolation (invariants #1/#10): everything here is control-plane tenancy on
//! tokio — composition reads read-mostly stores, persistence writes files,
//! and the notify is a one-permit coalescing signal that can never queue,
//! grow, or block. The render thread never sees any of it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_config::{MultiviewConfig, StartMode};

use crate::error::{ControlError, ControlResult};
use crate::state::AppState;

/// The state directory name under the boot config file's directory.
const STATE_DIR: &str = ".multiview";
/// The persisted Loaded snapshot's file name.
const LOADED_FILE: &str = "loaded.toml";
/// The persisted Running state's file name.
const ACTIVE_FILE: &str = "active.toml";

/// The run's Boot/Loaded/Running model (ADR-W022): the boot path, the
/// immutable Loaded snapshot, the cold-start policy, and whether this run
/// actually resumed (with the honest fallback reason when it could not).
#[derive(Debug)]
pub struct BootModel {
    /// The boot config file path (the watch target and the promote target).
    boot_path: PathBuf,
    /// The immutable Loaded snapshot (the boot document at process start).
    loaded: MultiviewConfig,
    /// The `[control] start` policy the boot file declared.
    start: StartMode,
    /// Whether this run started from a valid persisted `active.toml`.
    resumed: bool,
    /// Why a `start = "resume"` run fell back to boot, if it did.
    resume_fallback: Option<String>,
    /// Unix milliseconds of the last successful `active.toml` write
    /// (`0` = never written this run).
    active_written_ms: AtomicI64,
    /// Whether a shed (partial) revert-to-start raised the
    /// `config-file-apply-incomplete` warning (ADR-W022 §5): a revert that
    /// later completes clears exactly the instance this run's revert raised
    /// — never the watcher's own latched instance.
    revert_incomplete: AtomicBool,
    /// The ticket of the last `active.toml` content actually written, under
    /// the lock that serializes EVERY boot-model file write (review M2): an
    /// aborted persist task's `spawn_blocking` write keeps running detached,
    /// so the next writer must both wait for it (single-writer on the
    /// deterministic temp names) and out-order it (a stale composition must
    /// never overwrite newer content).
    write_serial: std::sync::Mutex<u64>,
    /// The monotonically increasing write-ticket source (taken at compose
    /// time, compared in [`write_active_serialized`]).
    write_tickets: AtomicU64,
}

impl BootModel {
    /// Build the model for a run started from `boot_path` whose Loaded
    /// snapshot is `loaded`, under cold-start policy `start`. `resumed` /
    /// `resume_fallback` record what the resume resolution actually did.
    #[must_use]
    pub fn new(
        boot_path: PathBuf,
        loaded: MultiviewConfig,
        start: StartMode,
        resumed: bool,
        resume_fallback: Option<String>,
    ) -> Self {
        Self {
            boot_path,
            loaded,
            start,
            resumed,
            resume_fallback,
            active_written_ms: AtomicI64::new(0),
            revert_incomplete: AtomicBool::new(false),
            write_serial: std::sync::Mutex::new(0),
            write_tickets: AtomicU64::new(0),
        }
    }

    /// The boot config file path.
    #[must_use]
    pub fn boot_path(&self) -> &Path {
        &self.boot_path
    }

    /// The immutable Loaded snapshot (the revert-to-start target).
    #[must_use]
    pub fn loaded(&self) -> &MultiviewConfig {
        &self.loaded
    }

    /// The `[control] start` cold-start policy.
    #[must_use]
    pub fn start(&self) -> StartMode {
        self.start
    }

    /// Whether this run started from a valid persisted `active.toml`.
    #[must_use]
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// Why a `start = "resume"` run fell back to boot, if it did.
    #[must_use]
    pub fn resume_fallback(&self) -> Option<&str> {
        self.resume_fallback.as_deref()
    }

    /// The state directory (`<config-dir>/.multiview`).
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        state_dir_for(&self.boot_path)
    }

    /// The persisted Running state path (`<config-dir>/.multiview/active.toml`).
    #[must_use]
    pub fn active_path(&self) -> PathBuf {
        self.state_dir().join(ACTIVE_FILE)
    }

    /// The persisted Loaded snapshot path (`<config-dir>/.multiview/loaded.toml`).
    #[must_use]
    pub fn loaded_path(&self) -> PathBuf {
        self.state_dir().join(LOADED_FILE)
    }

    /// Unix milliseconds of the last successful `active.toml` write this run,
    /// or [`None`] when nothing has been persisted yet.
    #[must_use]
    pub fn active_written_ms(&self) -> Option<i64> {
        match self.active_written_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(ms),
        }
    }

    /// Record a successful `active.toml` write at `now_ms`.
    fn record_active_written(&self, now_ms: i64) {
        self.active_written_ms.store(now_ms, Ordering::Release);
    }

    /// Latch that a shed (partial) revert raised the
    /// `config-file-apply-incomplete` warning (ADR-W022 §5).
    pub fn note_revert_incomplete(&self) {
        self.revert_incomplete.store(true, Ordering::Release);
    }

    /// Take (and clear) the shed-revert latch: `true` when a previous revert
    /// raised the incomplete warning that a now-completed revert may clear.
    #[must_use]
    pub fn take_revert_incomplete(&self) -> bool {
        self.revert_incomplete.swap(false, Ordering::AcqRel)
    }

    /// Take the next monotonically increasing write ticket (call at compose
    /// time; pass to [`write_active_serialized`]).
    #[must_use]
    pub fn next_write_ticket(&self) -> u64 {
        self.write_tickets.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Lock the boot-model write serial, recovering from poisoning (a
    /// panicked writer must never wedge persistence).
    fn lock_writes(&self) -> std::sync::MutexGuard<'_, u64> {
        match self.write_serial.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// The state directory for a boot config at `boot_path`
/// (`<config-dir>/.multiview`; a bare filename resolves against `.`).
#[must_use]
pub fn state_dir_for(boot_path: &Path) -> PathBuf {
    match boot_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(STATE_DIR),
        _ => PathBuf::from(".").join(STATE_DIR),
    }
}

/// Load the persisted Running state (`active.toml`) next to `boot_path` as a
/// resume candidate: it must read, parse, AND validate.
///
/// # Errors
///
/// A human-readable reason naming the failure (missing/unreadable file, TOML
/// parse error, validation error) — the caller warns with it and falls back
/// to the boot document (pin (f)).
pub fn load_resume_config(boot_path: &Path) -> Result<MultiviewConfig, String> {
    let active = state_dir_for(boot_path).join(ACTIVE_FILE);
    let text = std::fs::read_to_string(&active).map_err(|e| {
        format!(
            "the persisted Running state {} cannot be read: {e}",
            active.display()
        )
    })?;
    let config = MultiviewConfig::load_from_toml(&text).map_err(|e| {
        format!(
            "the persisted Running state {} does not parse as TOML: {e}",
            active.display()
        )
    })?;
    config.validate().map_err(|e| {
        format!(
            "the persisted Running state {} does not validate: {e}",
            active.display()
        )
    })?;
    Ok(config)
}

/// Write `content` to `path` atomically: same-directory temp file →
/// `fsync(2)` the file → `rename(2)` over the destination → `fsync` the
/// directory. Readers always observe either the old or the new content, and a
/// successful write leaves no temp residue. An existing destination's
/// permission mode is preserved across the replace (review M3: a `chmod 600`
/// boot/state file must never silently widen to the umask default).
///
/// Single-writer by design (the one persister task / startup path): the temp
/// name is deterministic (`.<name>.tmp`), so a crash mid-write leaves at most
/// one temp file that the next successful write replaces.
///
/// # Errors
///
/// Any I/O error from create/write/chmod/sync/rename (a destination with no
/// parent directory or file name is [`std::io::ErrorKind::InvalidInput`]).
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("atomic write target {} has no file name", path.display()),
        )
    })?;
    // Stat the DESTINATION first: when it exists, the temp file inherits its
    // mode before the rename, so the replace preserves a tightened mode
    // (chmod-600 stays 600). A missing destination keeps the process default.
    let dest_permissions = std::fs::metadata(path).ok().map(|meta| meta.permissions());
    let tmp = dir.join(format!(".{}.tmp", name.to_string_lossy()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        // Tighten the mode BEFORE the content lands (review M3 residual): a
        // chmod-600 destination's secrets must never sit world-readable in
        // the temp file even for the write's duration.
        if let Some(permissions) = dest_permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(content.as_bytes())?;
        // Durability before visibility: the rename must never expose a file
        // whose bytes are still only in the page cache.
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        // Never leave the temp file behind on a failed rename.
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    // fsync the directory so the rename itself survives a power cut. A
    // filesystem that cannot fsync a directory handle (rare) degrades to the
    // rename's own guarantees rather than failing the (already-visible) write.
    if let Ok(dir_handle) = std::fs::File::open(&dir) {
        let _ = dir_handle.sync_all();
    }
    Ok(())
}

/// Persist the Loaded snapshot to `loaded.toml` (atomic, machine-written
/// canonical TOML), creating the state directory on demand.
///
/// # Errors
///
/// A human-readable reason on render or I/O failure — the caller warns and
/// continues (the in-memory Loaded snapshot is authoritative; the file is the
/// forensic copy).
pub fn persist_loaded(model: &BootModel) -> Result<(), String> {
    let toml = model
        .loaded
        .to_toml()
        .map_err(|e| format!("rendering the Loaded snapshot: {e}"))?;
    let dir = model.state_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("creating the state dir {}: {e}", dir.display()))?;
    write_atomic(&model.loaded_path(), &toml)
        .map_err(|e| format!("writing {}: {e}", model.loaded_path().display()))
}

/// Write `toml` to `active.toml` under the model's write lock, skipping the
/// write when a NEWER ticket already landed (review M2). Every boot-model
/// file write serializes on this lock: an aborted persist task's
/// `spawn_blocking` write keeps running detached on the blocking pool, so
/// the lock makes concurrent writers single-file on the deterministic temp
/// name, and the ticket check makes content monotonic — a stale composition
/// can never overwrite newer state. Returns whether this content was
/// actually written.
///
/// # Errors
///
/// Any I/O error from `create_dir_all` or the atomic write. Blocking I/O —
/// call from `spawn_blocking` on async paths.
pub fn write_active_serialized(
    model: &BootModel,
    ticket: u64,
    toml: &str,
) -> std::io::Result<bool> {
    let mut last = model.lock_writes();
    if *last > ticket {
        // A newer composition already landed; this one is stale.
        return Ok(false);
    }
    std::fs::create_dir_all(model.state_dir())?;
    write_atomic(&model.active_path(), toml)?;
    *last = ticket;
    Ok(true)
}

/// Write `toml` to the BOOT file under the model's write lock, on the
/// blocking pool (reviews m1 + M2): the promote route's write never parks
/// the control-plane reactor and never interleaves with another boot-model
/// file write on a deterministic temp name.
///
/// # Errors
///
/// Any I/O error from the atomic write (or a failed blocking task).
pub async fn write_boot_file(model: Arc<BootModel>, toml: String) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let _serial = model.lock_writes();
        write_atomic(model.boot_path(), &toml)
    })
    .await
    .map_err(|e| std::io::Error::other(format!("the boot-file write task failed: {e}")))?
}

/// Persist the CURRENT Running state to `active.toml`, now: compose the
/// running document via the SAME composition the export route uses,
/// deserialize + validate it (so an `active.toml` that exists always
/// round-trips `MultiviewConfig::validate` — pin (d)), render canonical TOML,
/// and write it atomically. A run without a boot model persists nothing.
///
/// The composition reads read-mostly stores in place; the file I/O rides
/// [`tokio::task::spawn_blocking`] (review m1) through
/// [`write_active_serialized`] (review M2: locked + ticket-ordered, so a
/// detached stale write can neither interleave with nor overwrite a newer
/// one).
///
/// # Errors
///
/// Any composition/validation/render fault as the export route would surface
/// it, or [`ControlError::Repository`] for an I/O failure. Callers on the
/// persist task treat every error as fail-soft (warn + skip).
pub async fn persist_running_now(state: &AppState) -> ControlResult<()> {
    let Some(model) = state.boot_model.as_ref() else {
        return Ok(());
    };
    let (_document, config) = crate::routes::config::compose_running_config(state)?;
    let toml = config
        .to_toml()
        .map_err(|e| ControlError::Repository(format!("TOML render failed: {e}")))?;
    // The ticket is taken AFTER composing, so ticket order tracks content
    // freshness: a slower, older composition loses to a newer one.
    let ticket = model.next_write_ticket();
    let task_model = Arc::clone(model);
    let written =
        tokio::task::spawn_blocking(move || write_active_serialized(&task_model, ticket, &toml))
            .await
            .map_err(|e| ControlError::Repository(format!("the persist write task failed: {e}")))?
            .map_err(|e| {
                ControlError::Repository(format!("writing {}: {e}", model.active_path().display()))
            })?;
    if written {
        model.record_active_written(state.ack_now().as_nanos().div_euclid(1_000_000));
    }
    Ok(())
}

/// Stop the debounced Running persister at run teardown (review M2): abort
/// the task, await it, and run one final best-effort [`persist_running_now`]
/// to capture changes younger than the debounce.
///
/// `task.await` returns once the TASK is terminated — but a `spawn_blocking`
/// write the task started keeps running detached on the blocking pool, so
/// awaiting the task alone is NOT a single-writer guarantee. The guarantee
/// comes from [`write_active_serialized`]: every active-file write holds the
/// model's write lock (no interleaving on the deterministic temp name) and
/// carries a compose-time ticket (the final persist's newer content can
/// never be overwritten by the detached stale write). A persist failure is
/// warned and teardown continues (fail-soft).
pub async fn finish_running_persist(task: tokio::task::JoinHandle<()>, state: &AppState) {
    task.abort();
    let _ = task.await;
    if let Err(error) = persist_running_now(state).await {
        tracing::warn!(
            error = %error,
            "the final running-state persist at shutdown was skipped (fail-soft)"
        );
    }
}

/// Spawn the debounced Running persister (ADR-W022 §3): persist the starting
/// Running state once (a stale `active.toml` from a previous run never
/// outlives the run that supersedes it), then wait on the `running_changed`
/// notify the audit choke point fires, sleep the `debounce` (at most one
/// write per window), and persist. Every failure is a `tracing::warn!` and a
/// skipped write — the task never exits on error (fail-soft) and nothing it
/// does can back-pressure the engine (invariant #10).
///
/// The caller stops the returned handle at teardown via
/// [`finish_running_persist`] (abort → await → one final best-effort persist
/// capturing changes younger than the debounce).
#[must_use]
pub fn spawn_running_persist(state: AppState, debounce: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = persist_running_now(&state).await {
            tracing::warn!(
                error = %error,
                "running-state persist: the startup write was skipped (fail-soft)"
            );
        }
        loop {
            state.running_changed.notified().await;
            tokio::time::sleep(debounce).await;
            if let Err(error) = persist_running_now(&state).await {
                tracing::warn!(
                    error = %error,
                    "running-state persist: this write was skipped (fail-soft)"
                );
            }
        }
    })
}
