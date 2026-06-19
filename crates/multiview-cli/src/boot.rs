//! Cold-start resolution for the Boot/Loaded/Running model (ADR-W024 §4).
//!
//! `multiview run` starts from its config file (**Boot**). The boot file's
//! `[control] start` policy decides where the starting **Running** state comes
//! from:
//!
//! * `start = "boot"` (the default): Running is the boot document itself; any
//!   persisted `active.toml` from a previous run is ignored (and superseded by
//!   this run's first persist).
//! * `start = "resume"`: Running is the last persisted
//!   `<config-dir>/.multiview/active.toml` when it reads, parses, AND
//!   validates — otherwise the run falls back to the boot document with a
//!   warning naming the reason (surfaced on `GET /api/v1/config/boot-model`
//!   as `resume_fallback`).
//!
//! **Loaded** stays the boot snapshot in BOTH modes: revert-to-start targets
//! the deliberate cold-start baseline, never the resumed state. The boot file
//! also stays the ADR-W020 watch target, so an external boot-file edit during
//! a resumed run still hot-applies (diffed against the resumed baseline).

use std::path::Path;

use multiview_config::{MultiviewConfig, StartMode};
use multiview_control::boot_model::{load_resume_config, BootModel};

/// The resolved starting state of a run: the document the engine is built
/// from (**Running**), the immutable boot snapshot (**Loaded**), and what the
/// resume resolution actually did.
#[derive(Debug, Clone)]
pub struct StartConfig {
    /// The starting Running state: the engine is built from it, the control
    /// stores are seeded from it, and the ADR-W020 watcher's baseline is it.
    pub running: MultiviewConfig,
    /// The immutable Loaded snapshot (the boot document at process start) —
    /// the revert-to-start target in both modes.
    pub loaded: MultiviewConfig,
    /// The raw boot-file TEXT as read at process start: the watcher's
    /// initial last-observed content (review m4 — under a resume the
    /// UNCHANGED boot file must never clobber the resumed baseline; only a
    /// real content change applies, and an edit landing in the boot window
    /// IS a content change against this text).
    pub boot_text: String,
    /// The `[control] start` policy the boot file declared.
    pub start: StartMode,
    /// Whether Running actually came from a valid persisted `active.toml`.
    pub resumed: bool,
    /// Why a `start = "resume"` run fell back to boot, if it did.
    pub resume_fallback: Option<String>,
}

impl StartConfig {
    /// The control plane's [`BootModel`] for this resolution, rooted at the
    /// boot file `boot_path` (the watch + promote target).
    #[must_use]
    pub fn to_boot_model(&self, boot_path: &Path) -> BootModel {
        BootModel::new(
            boot_path.to_path_buf(),
            self.loaded.clone(),
            self.start,
            self.resumed,
            self.resume_fallback.clone(),
        )
    }
}

/// Splice the storeless restart-only sections from the BOOT document into a
/// resumed Running document (ADR-W024 review M1): `control`, `placement`,
/// `salvos`, `tally_profiles`, `walls`, `routing`, and `schema_version` have
/// no control store — the boot file is their durable truth, and a restart is
/// exactly when they take effect. Without the splice, a boot-file
/// `[control] listen` edit would be silently lost on the very restart the
/// operator performed to apply it (the stale `active.toml` copy would win).
fn splice_storeless_sections(
    mut running: MultiviewConfig,
    boot: &MultiviewConfig,
) -> MultiviewConfig {
    running.schema_version = boot.schema_version;
    running.control.clone_from(&boot.control);
    running.placement.clone_from(&boot.placement);
    running.salvos.clone_from(&boot.salvos);
    running.tally_profiles.clone_from(&boot.tally_profiles);
    running.walls.clone_from(&boot.walls);
    running.routing.clone_from(&boot.routing);
    running
}

/// Resolve the starting Running state for a run booted from `boot` (already
/// parsed + validated) at `boot_path` (ADR-W024 §4).
///
/// Under `start = "resume"` the persisted `active.toml` next to the boot file
/// becomes Running when it is valid — with the storeless restart-only
/// sections (`control`, `placement`, `salvos`, `tally_profiles`, `walls`,
/// `routing`, `schema_version`) spliced from the BOOT document (review M1: a
/// boot-file edit to a restart-only section must take effect on restart). A
/// missing/unreadable/invalid file — or a splice that no longer validates —
/// falls back to the boot document with a `tracing::warn!` and the reason
/// recorded in [`StartConfig::resume_fallback`]. The default `boot` policy
/// never reads `active.toml`.
#[must_use]
pub fn resolve_start_config(
    boot: MultiviewConfig,
    boot_text: String,
    boot_path: &Path,
) -> StartConfig {
    let start = boot
        .control
        .as_ref()
        .map_or(StartMode::Boot, |control| control.start);
    if start != StartMode::Resume {
        return StartConfig {
            running: boot.clone(),
            loaded: boot,
            boot_text,
            start,
            resumed: false,
            resume_fallback: None,
        };
    }
    match load_resume_config(boot_path) {
        Ok(active) => {
            let running = splice_storeless_sections(active, &boot);
            match running.validate() {
                Ok(()) => {
                    tracing::info!(
                        boot = %boot_path.display(),
                        "start = \"resume\": starting from the persisted Running state \
                         (active.toml) with the restart-only sections from the boot \
                         file; the boot file stays the Loaded snapshot and the watch \
                         target"
                    );
                    StartConfig {
                        running,
                        loaded: boot,
                        boot_text,
                        start,
                        resumed: true,
                        resume_fallback: None,
                    }
                }
                Err(error) => {
                    let reason = format!(
                        "the persisted Running state does not validate once the boot \
                         file's restart-only sections are spliced in: {error}"
                    );
                    tracing::warn!(
                        boot = %boot_path.display(),
                        reason = %reason,
                        "start = \"resume\" requested but the spliced Running state is \
                         unusable; falling back to the boot document"
                    );
                    StartConfig {
                        running: boot.clone(),
                        loaded: boot,
                        boot_text,
                        start,
                        resumed: false,
                        resume_fallback: Some(reason),
                    }
                }
            }
        }
        Err(reason) => {
            tracing::warn!(
                boot = %boot_path.display(),
                reason = %reason,
                "start = \"resume\" requested but the persisted Running state is \
                 unusable; falling back to the boot document"
            );
            StartConfig {
                running: boot.clone(),
                loaded: boot,
                boot_text,
                start,
                resumed: false,
                resume_fallback: Some(reason),
            }
        }
    }
}
