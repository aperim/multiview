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
//! Isolation (invariants #1 + #10): every engine submission is the
//! non-blocking `try_submit` (a full bus sheds with a warning — re-saving the
//! file retries), every publish is the drop-oldest event broadcast, and every
//! store touched is read-mostly control-plane state. Nothing here can pace,
//! stall, or back-pressure the output clock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multiview_config::{ConfigDiff, MultiviewConfig, SourceChange};
use multiview_control::{
    resolve_layout_document, AppState, AuditAction, Command, LayoutInput, OperationId,
    ResourceInput, ResourceRepository,
};
use multiview_events::{Event, HealthWarning, WarningCode, WarningSeverity};

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
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
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
}

/// The cloneable handle [`spawn`] returns: self-write suppression for the
/// promote-to-boot lane ([`ConfigWatchHandle::expect_write`]) and a stop flag
/// for run teardown.
#[derive(Debug, Clone)]
pub struct ConfigWatchHandle {
    /// Outstanding expected (server-side) writes; each suppresses one reload.
    expected: Arc<AtomicU64>,
    /// Raised by [`ConfigWatchHandle::stop`]; the loop exits on its next poll.
    stop: Arc<AtomicBool>,
}

impl ConfigWatchHandle {
    /// Mark one **expected** write: the next debounced file change is adopted
    /// as the new baseline WITHOUT applying anything (the server-side writer —
    /// e.g. a promote-to-boot flow — already applied the state it serialized).
    /// Call immediately before writing the file. Each call suppresses exactly
    /// one reload.
    pub fn expect_write(&self) {
        self.expected.fetch_add(1, Ordering::AcqRel);
    }

    /// Stop the watcher (it exits on its next poll tick).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Consume one outstanding expected-write token, if any.
    fn consume_expected(&self) -> bool {
        self.expected
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
    }
}

/// Spawn the config-file watcher over `path` on the control-plane tokio
/// runtime. `baseline` is the document the run booted with (the currently
/// RUNNING state); `state` is the router's `AppState` (the one set of stores,
/// the bounded command bus, the drop-oldest publisher, the audit log, and the
/// watch-status slot).
pub fn spawn(
    path: PathBuf,
    baseline: MultiviewConfig,
    state: AppState,
    options: WatchOptions,
) -> ConfigWatchHandle {
    let handle = ConfigWatchHandle {
        expected: Arc::new(AtomicU64::new(0)),
        stop: Arc::new(AtomicBool::new(false)),
    };
    state.config_watch.mark_active(&path.display().to_string());
    tracing::info!(
        path = %path.display(),
        poll = ?options.poll_interval,
        "watching the config file for external changes (ADR-W020)"
    );
    // Fingerprint the file NOW, synchronously: `baseline` was loaded from this
    // content, so the baseline fingerprint must be captured before the spawned
    // task first runs (a write landing in that window must trigger a reload,
    // not be silently adopted as the baseline).
    let applied = probe(&path);
    let task_handle = handle.clone();
    tokio::spawn(watch_loop(
        path,
        baseline,
        applied,
        state,
        options,
        task_handle,
    ));
    handle
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
/// missing.
fn probe(path: &Path) -> Option<Fingerprint> {
    let meta = std::fs::metadata(path).ok()?;
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
async fn watch_loop(
    path: PathBuf,
    mut baseline: MultiviewConfig,
    // The fingerprint of the content `baseline` reflects (captured at spawn).
    mut applied: Option<Fingerprint>,
    state: AppState,
    options: WatchOptions,
    handle: ConfigWatchHandle,
) {
    let mut candidate: Option<Fingerprint> = None;
    let mut missing_polls: u32 = 0;
    let mut missing_reported = false;
    // Whether a `config-file-invalid` warning is currently raised (cleared on
    // the next valid apply).
    let mut invalid_active = false;
    loop {
        tokio::time::sleep(options.poll_interval).await;
        if handle.stop.load(Ordering::Acquire) {
            tracing::debug!(path = %path.display(), "config-file watcher stopped");
            return;
        }
        let Some(now) = probe(&path) else {
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
            continue;
        }
        if candidate.as_ref() != Some(&now) {
            // First sighting of this fingerprint: wait one more poll for the
            // writer to settle (editors multi-write).
            candidate = Some(now);
            continue;
        }
        // Stable across two polls: act on it.
        candidate = None;
        applied = Some(now);
        if handle.consume_expected() {
            adopt_expected_write(&path, &mut baseline);
            continue;
        }
        apply_change(&path, &mut baseline, &state, &mut invalid_active);
    }
}

/// Adopt an **expected** (server-side) write as the new baseline without
/// applying anything — the writer already applied the state it serialized. A
/// write that does not parse/validate is still warned (a buggy writer must
/// never be silent), and the baseline is kept.
fn adopt_expected_write(path: &Path, baseline: &mut MultiviewConfig) {
    match load_validated(path) {
        Ok(next) => {
            tracing::debug!(
                path = %path.display(),
                "expected (self) config write adopted as the new baseline; no reload"
            );
            *baseline = next;
        }
        Err(reason) => {
            tracing::warn!(
                path = %path.display(),
                reason = %reason,
                "an EXPECTED config write does not validate — the writer is buggy; \
                 keeping the previous baseline"
            );
        }
    }
}

/// Read + parse + validate the whole document, with a human-readable reason on
/// any failure.
fn load_validated(path: &Path) -> Result<MultiviewConfig, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("the file cannot be read: {e}"))?;
    let next = MultiviewConfig::load_from_toml(&text)
        .map_err(|e| format!("the document does not parse: {e}"))?;
    next.validate()
        .map_err(|e| format!("the document does not validate: {e}"))?;
    Ok(next)
}

/// Handle one settled external change: whole-document validate, per-section
/// diff, apply through the one machinery, advance the baseline.
fn apply_change(
    path: &Path,
    baseline: &mut MultiviewConfig,
    state: &AppState,
    invalid_active: &mut bool,
) {
    let next = match load_validated(path) {
        Ok(next) => next,
        Err(reason) => {
            reject(state, path, &reason, invalid_active);
            return;
        }
    };
    let diff = ConfigDiff::between(baseline, &next);
    if diff.is_empty() {
        // A touch / rewrite with identical content: adopt silently.
        *baseline = next;
        clear_invalid(state, path, invalid_active);
        tracing::debug!(path = %path.display(), "config file rewritten with identical content");
        return;
    }
    let outcome = apply_diff(state, &diff, &next);
    *baseline = next;
    clear_invalid(state, path, invalid_active);
    state
        .config_watch
        .record_applied(now_ms(state), &outcome.summary);
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
        summary = %outcome.summary,
        "applied an external config-file change (ADR-W020)"
    );
}

/// What one file apply did: the human summary and the restart-only sections.
struct ApplyOutcome {
    summary: String,
    restart: BTreeSet<String>,
}

/// Apply a validated per-section diff through the one apply machinery:
/// engine commands for the live-appliable parts, store reseeds for everything
/// the UI mirrors, restart-section accounting for the rest.
fn apply_diff(state: &AppState, diff: &ConfigDiff, next: &MultiviewConfig) -> ApplyOutcome {
    let mut restart: BTreeSet<String> = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();

    // 1. Sources FIRST (FIFO bus): a layout rebinding to a just-added source
    //    must find its store registered in the same frame-boundary pass.
    if !diff.sources.is_empty() {
        parts.push(apply_source_changes(state, diff, &mut restart));
        resync_store(state, &state.sources, &desired_sources(next));
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
        parts.push(apply_layout_change(state, diff, next, &mut restart));
    }

    // 4. Every other changed section: reseed its store where one exists (the
    //    UI's truth follows the file) + restart accounting (no live path yet).
    for section in &diff.changed_sections {
        restart.insert((*section).to_owned());
        match *section {
            "outputs" => resync_store(state, &state.outputs, &desired_outputs(next)),
            "overlays" => resync_store(state, &state.overlays, &desired_overlays(next)),
            "probes" => resync_store(state, &state.probes, &desired_probes(next)),
            "devices" => {
                resync_store(state, &state.devices, &desired_devices(next));
                for device in &next.devices {
                    state.device_status.ensure(&device.id);
                }
            }
            "sync_groups" => resync_store(state, &state.sync_groups, &desired_sync_groups(next)),
            "audio" => resync_audio(state, next),
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
        summary: parts.join("; "),
        restart,
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
) -> String {
    let mut described: Vec<String> = Vec::new();
    for change in &diff.sources {
        match change {
            SourceChange::Added(source) => {
                described.push(format!("{} added", source.id));
                if source.kind.is_synthetic() {
                    submit(
                        state,
                        Command::UpsertSource {
                            op: OperationId::new(),
                            source: source.clone(),
                        },
                    );
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
                    submit(
                        state,
                        Command::UpsertSource {
                            op: OperationId::new(),
                            source: next.clone(),
                        },
                    );
                } else {
                    if previous.kind.is_synthetic() {
                        // Mirror the sources route: stop the stale generator
                        // now; a frozen synthetic pretending to be the new
                        // URL would be dishonest.
                        submit(
                            state,
                            Command::RemoveSource {
                                op: OperationId::new(),
                                id: next.id.clone(),
                            },
                        );
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
                submit(
                    state,
                    Command::RemoveSource {
                        op: OperationId::new(),
                        id: id.clone(),
                    },
                );
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
    diff: &ConfigDiff,
    next: &MultiviewConfig,
    restart: &mut BTreeSet<String>,
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
    reseed_working_layout(state, &id, &body);
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
            submit(
                state,
                Command::ApplyLayout {
                    op: OperationId::new(),
                    layout: id,
                    document: Some(Box::new(resolved)),
                },
            );
            "layout applied live".to_owned()
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
fn reseed_working_layout(state: &AppState, id: &str, body: &serde_json::Value) {
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
                ACTOR,
                AuditAction::Update,
                multiview_control::repository::LAYOUT_KIND,
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

/// Bring a resource store's contents in line with the file's desired state:
/// create/update/delete by id, audit-logged as actor `config-file`. A store
/// fault is warned and skipped — a flaky store must never wedge the watcher.
fn resync_store(
    state: &AppState,
    store: &Arc<dyn ResourceRepository>,
    desired: &[(String, ResourceInput)],
) {
    let existing: Vec<String> = match store.list() {
        Ok(list) => list.into_iter().map(|v| v.resource.id).collect(),
        Err(error) => {
            tracing::warn!(kind = store.kind(), error = %error, "config-file resync: list failed");
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
            Ok(action) => state.audit(ACTOR, action, store.kind(), id, Some(input.body.clone())),
            Err(error) => {
                tracing::warn!(kind = store.kind(), id = %id, error = %error, "config-file resync: write failed");
            }
        }
    }
    for id in existing {
        if !desired.iter().any(|(want, _)| want == &id) {
            match store.delete(&id) {
                Ok(()) => state.audit(ACTOR, AuditAction::Delete, store.kind(), &id, None),
                Err(error) => {
                    tracing::warn!(kind = store.kind(), id = %id, error = %error, "config-file resync: delete failed");
                }
            }
        }
    }
}

/// Replace the audio-routing singleton from the file's `[audio]` block (a
/// bounded CAS loop over the versioned store). A REMOVED block cannot be
/// expressed on the singleton store today; the restart warning covers it.
fn resync_audio(state: &AppState, next: &MultiviewConfig) {
    let Some(routing) = next.audio.clone() else {
        tracing::warn!(
            "config file removed the [audio] block; the audio-routing store keeps its last \
             value until restart"
        );
        return;
    };
    let mut version = state.audio_routing.version();
    for _ in 0_u8..4 {
        match state.audio_routing.replace_if(version, routing.clone()) {
            Ok(_) => {
                state.audit(
                    ACTOR,
                    AuditAction::Update,
                    multiview_control::AUDIO_ROUTING_KIND,
                    multiview_control::AUDIO_ROUTING_ID,
                    to_body(&routing),
                );
                return;
            }
            Err(actual) => version = actual,
        }
    }
    tracing::warn!("config-file resync: the audio-routing store kept changing; giving up");
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
            tracing::warn!(error = %error, "config-file resync: serializing a config value failed");
            None
        }
    }
}

/// Submit a command on the bounded, non-blocking bus; a full/closed bus sheds
/// with a warning (invariant #10) — re-saving the file retries.
fn submit(state: &AppState, command: Command) {
    let kind = command.kind();
    if let Err(error) = state.commands.try_submit(command) {
        tracing::warn!(
            command = kind,
            error = %error,
            "config-file apply: the engine command bus shed this change \
             (re-save the file to retry)"
        );
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

/// Clear a previously-raised `config-file-invalid` warning after a valid apply.
fn clear_invalid(state: &AppState, path: &Path, invalid_active: &mut bool) {
    if !*invalid_active {
        return;
    }
    *invalid_active = false;
    state
        .engine
        .publish_event(Event::HealthWarningCleared(invalid_warning(
            path,
            "a subsequent valid write applied",
            now_nanos(state),
            false,
        )));
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
         differs from the file until then"
    );
    state
        .engine
        .publish_event(Event::HealthWarningRaised(HealthWarning {
            code: WarningCode::ConfigFileRequiresRestart,
            severity: WarningSeverity::Warning,
            subsystem: "config".to_owned(),
            message: format!(
                "The config file changed section(s) [{sections}] that cannot hot-apply; \
                 the running process differs from the file until a restart."
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
