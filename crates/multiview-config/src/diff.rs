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
use crate::schema::Source;
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
    /// `schema_version`). Sorted + deduplicated.
    pub changed_sections: BTreeSet<&'static str>,
}

impl ConfigDiff {
    /// Compute the structural diff from the `running` document to `next`.
    #[must_use]
    pub fn between(running: &MultiviewConfig, next: &MultiviewConfig) -> Self {
        // Canvas: the pinned signal is geometry + cadence BY VALUE (the
        // LayoutCanvas PartialEq cross-multiplies Fps), exactly the ADR-W019
        // Class-1 gate's comparison. Everything else on the canvas is
        // cosmetic (and still restart-only).
        let running_signal = LayoutCanvas::new(
            running.canvas.width,
            running.canvas.height,
            running.canvas.fps,
        );
        let next_signal = LayoutCanvas::new(next.canvas.width, next.canvas.height, next.canvas.fps);
        let canvas_signal_changed = running_signal != next_signal;
        let canvas_cosmetic_changed = !canvas_signal_changed && running.canvas != next.canvas;

        let mut changed_sections = BTreeSet::new();
        let sectioned: [(&'static str, bool); 13] = [
            (
                "schema_version",
                running.schema_version != next.schema_version,
            ),
            ("outputs", running.outputs != next.outputs),
            ("overlays", running.overlays != next.overlays),
            ("probes", running.probes != next.probes),
            ("audio", running.audio != next.audio),
            ("control", running.control != next.control),
            ("placement", running.placement != next.placement),
            ("salvos", running.salvos != next.salvos),
            (
                "tally_profiles",
                running.tally_profiles != next.tally_profiles,
            ),
            ("walls", running.walls != next.walls),
            ("devices", running.devices != next.devices),
            ("sync_groups", running.sync_groups != next.sync_groups),
            ("routing", running.routing != next.routing),
        ];
        for (name, changed) in sectioned {
            if changed {
                changed_sections.insert(name);
            }
        }

        Self {
            sources: diff_sources(running, next),
            canvas_signal_changed,
            canvas_cosmetic_changed,
            layout_changed: running.layout != next.layout || running.cells != next.cells,
            changed_sections,
        }
    }

    /// Whether the two documents were structurally identical.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && !self.canvas_signal_changed
            && !self.canvas_cosmetic_changed
            && !self.layout_changed
            && self.changed_sections.is_empty()
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
