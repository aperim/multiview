//! The pure per-section structural diff between two configuration documents
//! (ADR-W020).
//!
//! [`ConfigDiff::between`] compares a **running** (currently-applied) document
//! against a **next** (just-loaded) one and reports, per section, exactly what
//! changed — the unit the config-file watcher turns into apply actions: source
//! changes ride the live `UpsertSource`/`RemoveSource` machinery, a
//! layout/cells change rides the apply-layout path, a pinned-canvas change is
//! Class-2 (restart), and every other changed section is named for the
//! reseed + requires-restart surface.
//!
//! Pure and total: no I/O, no clocks, no allocation beyond the reported
//! changes. Comparison is over the typed schema (`PartialEq`), so semantic
//! equality holds where the schema defines it — e.g. an [`Fps`] cadence
//! compares **by value** (`50/2 == 25/1`), never by spelling.

use std::collections::BTreeSet;

use crate::layout_doc::LayoutCanvas;
use crate::schema::{Overlay, Source};
use crate::MultiviewConfig;

/// One source-level change between the running and next documents, keyed by
/// the source's stable `id`.
///
/// Deliberately **exhaustive** (no `#[non_exhaustive]`): added/changed/removed
/// is the complete taxonomy of an id-keyed list diff by construction, and the
/// watcher's apply plan must handle every case — an exhaustive `match` is the
/// compiler-enforced proof.
/// The `Source` payloads are boxed: a source document is large (~330 B) and a
/// `Removed` id is a thin `String`, so boxing keeps the enum small on the
/// stack (and is exactly the shape `Command::UpsertSource` carries).
#[derive(Debug, Clone, PartialEq)]
pub enum SourceChange {
    /// A source id present in `next` but not in `running`.
    Added(Box<Source>),
    /// A source id present in both whose document differs.
    Changed {
        /// The running document's version (e.g. for synthetic→decoded kind
        /// transitions, which stop the stale producer).
        previous: Box<Source>,
        /// The next document's version (the one to apply).
        next: Box<Source>,
    },
    /// A source id present in `running` but not in `next`.
    Removed(String),
}

/// One overlay-level change between the running and next documents, keyed by the
/// overlay's stable `id` (ADR-W024 round 7).
///
/// The file-watch overlay apply mirrors the source path: it derives the delta
/// from THIS list (computed against the stable file baseline by
/// [`ConfigDiff::between`]), NOT from the mutated control store — a store-derived
/// delta vanishes on a shed retry (the store is reseeded to `next` before the
/// retry resolves) and the change is silently lost. Exhaustive (no
/// `#[non_exhaustive]`): added/changed/removed is the complete id-keyed taxonomy.
/// The `Overlay` payloads are boxed (an overlay document is large; a `Removed`
/// id is a thin `String`) — exactly the shape `Command::UpsertOverlay` carries.
#[derive(Debug, Clone, PartialEq)]
pub enum OverlayChange {
    /// An overlay id present in `next` but not in `running`.
    Added(Box<Overlay>),
    /// An overlay id present in both whose document differs (the `next` version
    /// is the one to apply via `UpsertOverlay`).
    Changed(Box<Overlay>),
    /// An overlay id present in `running` but not in `next` (apply via
    /// `RemoveOverlay`).
    Removed(String),
}

/// The per-section structural diff between two configuration documents.
///
/// Produced by [`ConfigDiff::between`]; an all-default value (see
/// [`ConfigDiff::is_empty`]) means the documents are identical.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct ConfigDiff {
    /// Source changes by id: `Added`/`Changed` in `next` declaration order,
    /// then `Removed` in `running` declaration order (deterministic — and the
    /// apply order a layout rebinding to a just-added source needs).
    pub sources: Vec<SourceChange>,
    /// Overlay changes by id (ADR-W024 round 7): `Added`/`Changed` in `next`
    /// declaration order, then `Removed` in `running` declaration order. The
    /// file-watch overlay apply derives its `UpsertOverlay`/`RemoveOverlay`
    /// commands from THIS (baseline-derived, stable across shed retries), never
    /// from the mutated control store. `overlays` is ALSO in `changed_sections`
    /// when non-empty (so reporting/restart accounting stays uniform).
    pub overlays: Vec<OverlayChange>,
    /// The **pinned signal** (width / height / cadence-by-value) changed — a
    /// Class-2 change (ADR-R004): never hot-appliable.
    pub canvas_signal_changed: bool,
    /// A non-pinned canvas axis (pixel format, background, colour profile)
    /// changed — not live-rendered either, so restart-only, but distinct from
    /// the pinned signal for honest reporting.
    pub canvas_cosmetic_changed: bool,
    /// The layout strategy or the cells changed (a live apply-layout candidate
    /// when the pinned signal did not change).
    pub layout_changed: bool,
    /// Every other changed section, by its authored TOML name (`outputs`,
    /// `overlays`, `probes`, `audio`, `control`, `placement`, `salvos`,
    /// `tally_profiles`, `walls`, `devices`, `sync_groups`, `routing`,
    /// `discovery`, `timing`, `webrtc`, `schema_version`). Sorted + deduplicated.
    pub changed_sections: BTreeSet<&'static str>,
}

impl ConfigDiff {
    /// Compute the structural diff from the `running` document to `next`.
    #[must_use]
    pub fn between(running: &MultiviewConfig, next: &MultiviewConfig) -> Self {
        // EXHAUSTIVE destructure (no `..`): adding a field to `MultiviewConfig`
        // is a compile error HERE until this diff accounts for it — a new
        // section can never silently fall through the watcher (review m1).
        // `#[non_exhaustive]` does not apply within the defining crate.
        let MultiviewConfig {
            schema_version: running_schema_version,
            canvas: running_canvas,
            layout: running_layout,
            sources: _,
            cells: running_cells,
            overlays: running_overlays,
            outputs: running_outputs,
            probes: running_probes,
            tally_profiles: running_tally_profiles,
            salvos: running_salvos,
            walls: running_walls,
            devices: running_devices,
            sync_groups: running_sync_groups,
            control: running_control,
            placement: running_placement,
            audio: running_audio,
            routing: running_routing,
            discovery: running_discovery,
            timing: running_timing,
            webrtc: running_webrtc,
            system: running_system,
        } = running;
        let MultiviewConfig {
            schema_version: next_schema_version,
            canvas: next_canvas,
            layout: next_layout,
            sources: _,
            cells: next_cells,
            overlays: next_overlays,
            outputs: next_outputs,
            probes: next_probes,
            tally_profiles: next_tally_profiles,
            salvos: next_salvos,
            walls: next_walls,
            devices: next_devices,
            sync_groups: next_sync_groups,
            control: next_control,
            placement: next_placement,
            audio: next_audio,
            routing: next_routing,
            discovery: next_discovery,
            timing: next_timing,
            webrtc: next_webrtc,
            system: next_system,
        } = next;

        // Canvas: the pinned signal is geometry + cadence BY VALUE (the
        // LayoutCanvas PartialEq cross-multiplies Fps), exactly the ADR-W019
        // Class-1 gate's comparison. Everything else on the canvas is
        // cosmetic (and still restart-only).
        let running_signal = LayoutCanvas::new(
            running_canvas.width,
            running_canvas.height,
            running_canvas.fps,
        );
        let next_signal = LayoutCanvas::new(next_canvas.width, next_canvas.height, next_canvas.fps);
        let canvas_signal_changed = running_signal != next_signal;
        let canvas_cosmetic_changed = !canvas_signal_changed && running_canvas != next_canvas;

        let mut changed_sections = BTreeSet::new();
        let sectioned: [(&'static str, bool); 17] = [
            (
                "schema_version",
                running_schema_version != next_schema_version,
            ),
            ("outputs", running_outputs != next_outputs),
            ("overlays", running_overlays != next_overlays),
            ("probes", running_probes != next_probes),
            ("audio", running_audio != next_audio),
            ("control", running_control != next_control),
            ("placement", running_placement != next_placement),
            ("salvos", running_salvos != next_salvos),
            (
                "tally_profiles",
                running_tally_profiles != next_tally_profiles,
            ),
            ("walls", running_walls != next_walls),
            ("devices", running_devices != next_devices),
            ("sync_groups", running_sync_groups != next_sync_groups),
            ("routing", running_routing != next_routing),
            ("discovery", running_discovery != next_discovery),
            ("timing", running_timing != next_timing),
            ("webrtc", running_webrtc != next_webrtc),
            // A change to `[system.ndi]` license acceptance is restart-class: full
            // live revocation/acceptance propagation to running NDI I/O at the next
            // safe boundary (ADR-0008 §7.5) is a separate follow-up, so surface it
            // as a changed section (controlled reset) rather than claiming hot-apply.
            ("system", running_system != next_system),
        ];
        for (name, changed) in sectioned {
            if changed {
                changed_sections.insert(name);
            }
        }

        let mut diff = Self {
            sources: diff_sources(running, next),
            overlays: diff_overlays(running, next),
            canvas_signal_changed,
            canvas_cosmetic_changed,
            layout_changed: running_layout != next_layout || running_cells != next_cells,
            changed_sections,
        };
        // Defence in depth behind the compile-time destructure guard above.
        backstop_unrecognized(&mut diff, running == next);
        diff
    }

    /// Whether the two documents were structurally identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && self.overlays.is_empty()
            && !self.canvas_signal_changed
            && !self.canvas_cosmetic_changed
            && !self.layout_changed
            && self.changed_sections.is_empty()
    }
}

/// The review-m1 backstop: if the per-section comparison produced an EMPTY
/// diff for two documents that are NOT equal (`documents_equal == false`),
/// surface a named `"unrecognized"` restart section instead of letting the
/// change vanish silently. Normally unreachable — [`ConfigDiff::between`]
/// destructures `MultiviewConfig` exhaustively, so a new field is a compile
/// error there — this guards a future partial-compare regression at runtime.
fn backstop_unrecognized(diff: &mut ConfigDiff, documents_equal: bool) {
    if diff.is_empty() && !documents_equal {
        diff.changed_sections.insert("unrecognized");
    }
}

/// Diff the source lists by id: `Added`/`Changed` in `next` declaration order,
/// then `Removed` in `running` declaration order.
fn diff_sources(running: &MultiviewConfig, next: &MultiviewConfig) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    for source in &next.sources {
        match running.sources.iter().find(|r| r.id == source.id) {
            None => changes.push(SourceChange::Added(Box::new(source.clone()))),
            Some(previous) if previous != source => changes.push(SourceChange::Changed {
                previous: Box::new(previous.clone()),
                next: Box::new(source.clone()),
            }),
            Some(_) => {}
        }
    }
    for source in &running.sources {
        if !next.sources.iter().any(|n| n.id == source.id) {
            changes.push(SourceChange::Removed(source.id.clone()));
        }
    }
    changes
}

/// Diff the overlay lists by id (ADR-W024 round 7), the same shape as
/// [`diff_sources`]: `Added`/`Changed` in `next` declaration order, then
/// `Removed` in `running` declaration order. Computed against the file baseline
/// so the file-watch overlay apply re-derives a STABLE delta on every shed
/// retry (a store-derived delta vanishes once the store is reseeded to `next`).
fn diff_overlays(running: &MultiviewConfig, next: &MultiviewConfig) -> Vec<OverlayChange> {
    let mut changes = Vec::new();
    for overlay in &next.overlays {
        match running.overlays.iter().find(|r| r.id == overlay.id) {
            None => changes.push(OverlayChange::Added(Box::new(overlay.clone()))),
            Some(previous) if previous != overlay => {
                changes.push(OverlayChange::Changed(Box::new(overlay.clone())));
            }
            Some(_) => {}
        }
    }
    for overlay in &running.overlays {
        if !next.overlays.iter().any(|n| n.id == overlay.id) {
            changes.push(OverlayChange::Removed(overlay.id.clone()));
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{backstop_unrecognized, ConfigDiff};

    /// REVIEW m1 backstop: if the per-section comparison ever reports an
    /// EMPTY diff for two documents that are NOT equal (a future field missed
    /// by a partial compare — normally impossible: `between` destructures the
    /// config exhaustively, so a new field is a compile error), the backstop
    /// must surface a named "unrecognized" restart section rather than let
    /// the change vanish silently.
    #[test]
    fn an_empty_diff_for_unequal_documents_reports_unrecognized() {
        let mut diff = ConfigDiff::default();
        assert!(diff.is_empty());
        backstop_unrecognized(&mut diff, false);
        assert!(
            diff.changed_sections.contains("unrecognized"),
            "unequal documents with an empty diff must surface an unrecognized section"
        );
        assert!(!diff.is_empty());
    }

    /// The backstop never fires for genuinely identical documents, and never
    /// touches a non-empty diff.
    #[test]
    fn the_backstop_is_inert_for_equal_documents_and_nonempty_diffs() {
        let mut diff = ConfigDiff::default();
        backstop_unrecognized(&mut diff, true);
        assert!(diff.is_empty(), "equal documents stay an empty diff");

        let mut nonempty = ConfigDiff::default();
        nonempty.changed_sections.insert("outputs");
        backstop_unrecognized(&mut nonempty, false);
        assert!(
            !nonempty.changed_sections.contains("unrecognized"),
            "a non-empty diff already carries the change; no backstop entry"
        );
    }
}
