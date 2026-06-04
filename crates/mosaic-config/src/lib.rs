//! # mosaic-config
//!
//! Config & template schema (serde), validation, and config-as-code
//! import/export for the Mosaic engine.
//!
//! This crate parses the authored TOML (and the canonical JSON wire form)
//! described in `docs/templates/layout-and-config.md`, solves CSS-grid layouts
//! into normalized rectangles, and validates the document's semantic
//! invariants before it ever reaches the engine. It is pure Rust with no native
//! dependencies — it builds in the GPU-free CI baseline.
//!
//! ## Pipeline
//!
//! ```text
//! TOML/JSON ──load──▶ MosaicConfig ──validate──▶ solve_layout ──▶ mosaic_core::layout::Layout
//! ```
//!
//! - [`MosaicConfig::load_from_toml`] / [`MosaicConfig::load_from_json`] parse a
//!   document (rejecting a float `fps`, malformed track, etc. at parse time).
//! - [`MosaicConfig::validate`] enforces the semantic invariants: unique ids,
//!   every `cells.source.input_id` resolves to a declared source, every grid
//!   `area` exists, the cadence is usable, and the **solved** geometry passes
//!   [`mosaic_core::layout::Layout::validate`].
//! - [`MosaicConfig::solve_layout`] flattens the document into a validated
//!   [`mosaic_core::layout::Layout`] (canvas + normalized cells) for the engine.
//!
//! All unions are internally tagged by `kind` (never `untagged`) per ADR-0010.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod grid;
pub mod probe;
pub mod salvo;
pub mod schema;
pub mod tally;
pub mod wall;

use std::collections::HashSet;

use mosaic_core::layout::{Canvas as CoreCanvas, Cell as CoreCell, FitMode, Layout as CoreLayout};

pub use error::ConfigError;
pub use probe::{DetectionZone, Dwell, LoudnessTarget, Probe, ProbeKind};
pub use salvo::{Salvo, SourceRecall, TallyRecall, UmdRecall};
pub use schema::{
    Border, Canvas, CanvasColor, Cell, CellQos, CellSource, ColorOverride, Fps, Layout, Output,
    Overlay, Rect, RtspOptions, Source, SourceAuth, SourceKind,
};
pub use tally::{BitColor, IndexCell, TallyProfile};
pub use wall::{HeadConfig, WallBezel, WallConfig};

/// A complete Mosaic configuration document (config-as-code).
///
/// This is the whole-engine declarative state: canvas, layout strategy,
/// managed sources, cells, overlays, and outputs, plus a `schema_version` that
/// drives migration. It deserializes from TOML (human authoring) and JSON (the
/// canonical wire form) and round-trips losslessly between them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct MosaicConfig {
    /// Document schema version (drives migration).
    pub schema_version: u32,
    /// The output canvas.
    pub canvas: Canvas,
    /// The layout placement strategy.
    pub layout: Layout,
    /// Managed inputs (owners of ingest/decode/color/resilience).
    #[serde(default)]
    pub sources: Vec<Source>,
    /// Cells (tiles) and their source bindings.
    #[serde(default)]
    pub cells: Vec<Cell>,
    /// Overlay layers.
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    /// Output sinks/servers.
    #[serde(default)]
    pub outputs: Vec<Output>,
    /// Content-aware fault probes (black/freeze/silence/loudness), per cell.
    #[serde(default)]
    pub probes: Vec<Probe>,
    /// Tally profiles (bit↔colour palette, index↔cell address map).
    #[serde(default)]
    pub tally_profiles: Vec<TallyProfile>,
    /// Named atomic recalls (layout + source + tally + UMD).
    #[serde(default)]
    pub salvos: Vec<Salvo>,
    /// Multi-head video walls.
    #[serde(default)]
    pub walls: Vec<WallConfig>,
}

impl MosaicConfig {
    /// Parse a configuration document from TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::TomlParse`] if the text is not valid TOML matching
    /// the schema (including a float `fps`, an unknown enum `kind`, or a
    /// malformed `fps` string).
    pub fn load_from_toml(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::TomlParse(e.to_string()))
    }

    /// Parse a configuration document from JSON text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::JsonParse`] if the text is not valid JSON matching
    /// the schema.
    pub fn load_from_json(text: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(text).map_err(|e| ConfigError::JsonParse(e.to_string()))
    }

    /// Serialize this document to TOML text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::TomlSerialize`] if serialization fails.
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string(self).map_err(|e| ConfigError::TomlSerialize(e.to_string()))
    }

    /// Serialize this document to JSON text (the canonical wire form).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::JsonSerialize`] if serialization fails.
    pub fn to_json(&self) -> Result<String, ConfigError> {
        serde_json::to_string_pretty(self).map_err(|e| ConfigError::JsonSerialize(e.to_string()))
    }

    /// Validate every semantic invariant this crate owns.
    ///
    /// Enforces, in addition to [`mosaic_core::layout::Layout::validate`] on the
    /// **solved** geometry:
    /// - `schema_version` is supported;
    /// - source ids are unique and non-empty;
    /// - cell ids are unique and non-empty;
    /// - every grid cell's `area` exists in the grid's area map, and every cell
    ///   is placed by exactly one of `area`/`rect`;
    /// - every `cells.source.input_id` resolves to a declared source id;
    /// - at least one output is declared, each with a non-empty codec;
    /// - the canvas cadence is a usable rational (invariant #3);
    /// - every probe/tally-profile/salvo references a declared cell (and every
    ///   salvo source-recall references a declared source), and each is
    ///   internally consistent;
    /// - every declared video wall passes the core wall invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] (or a more specific variant from the
    /// grid solver / cadence parse) naming the first violated invariant.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version == 0 {
            return Err(ConfigError::Validation(
                "schema_version must be >= 1".to_owned(),
            ));
        }

        self.validate_unique_ids()?;
        self.validate_cell_bindings()?;
        self.validate_outputs()?;
        self.validate_probes()?;
        self.validate_tally_profiles()?;
        self.validate_salvos()?;
        self.validate_walls()?;

        // Solving + the core structural check covers geometry (rects in 0..1,
        // positive extent, valid cadence) and grid wiring (areas resolve).
        let layout = self.solve_layout()?;
        layout.validate().map_err(|e| match e {
            mosaic_core::Error::Config(msg) => ConfigError::Validation(msg),
            other => ConfigError::Validation(other.to_string()),
        })?;

        Ok(())
    }

    /// Flatten this document into a validated-shape [`mosaic_core::layout::Layout`].
    ///
    /// Grid cells are placed by solving the CSS grid; absolute cells use their
    /// declared `rect`. The returned layout is *structurally* assembled — call
    /// [`mosaic_core::layout::Layout::validate`] (or [`MosaicConfig::validate`])
    /// to enforce its invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Grid`] for an unsolvable grid, or
    /// [`ConfigError::Validation`] when a cell lacks a placement, references an
    /// unknown grid area, or a grid cell appears under a non-grid layout.
    pub fn solve_layout(&self) -> Result<CoreLayout, ConfigError> {
        let canvas = self.core_canvas();

        let grid_rects = match self.layout.as_grid_layout()? {
            Some(grid) => Some(grid::solve(&grid, self.canvas.width, self.canvas.height)?),
            None => None,
        };

        let mut cells = Vec::with_capacity(self.cells.len());
        for cell in &self.cells {
            let core_cell = Self::solve_cell(cell, grid_rects.as_deref())?;
            cells.push(core_cell);
        }

        Ok(CoreLayout {
            name: format!("schema_v{}", self.schema_version),
            canvas,
            cells,
        })
    }

    /// Build the core canvas (geometry + exact cadence) from this document.
    fn core_canvas(&self) -> CoreCanvas {
        let cadence = self.canvas.fps.rational();
        CoreCanvas {
            width: self.canvas.width,
            height: self.canvas.height,
            fps_num: cadence.num,
            fps_den: cadence.den,
        }
    }

    /// Resolve one schema cell into a core cell, placing it by grid area or
    /// absolute rect.
    fn solve_cell(
        cell: &Cell,
        grid_rects: Option<&[grid::AreaRect]>,
    ) -> Result<CoreCell, ConfigError> {
        let (x, y, w, h) = match (&cell.area, &cell.rect) {
            (Some(_), Some(_)) => {
                return Err(ConfigError::Validation(format!(
                    "cell {:?} declares both `area` and `rect`; choose exactly one",
                    cell.id
                )));
            }
            (Some(area), None) => {
                let rects = grid_rects.ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "cell {:?} uses grid `area` {area:?} but the layout is not a grid",
                        cell.id
                    ))
                })?;
                let found = rects.iter().find(|r| &r.name == area).ok_or_else(|| {
                    ConfigError::Validation(format!(
                        "cell {:?} references unknown grid area {area:?}",
                        cell.id
                    ))
                })?;
                (found.x, found.y, found.w, found.h)
            }
            (None, Some(rect)) => (rect.x, rect.y, rect.w, rect.h),
            (None, None) => {
                return Err(ConfigError::Validation(format!(
                    "cell {:?} has neither `area` nor `rect`",
                    cell.id
                )));
            }
        };

        Ok(CoreCell {
            x,
            y,
            w,
            h,
            z: cell.z,
            fit: parse_fit(cell.fit.as_deref()),
            source: cell.source.input_id.clone(),
            // Per-tile opacity: a fully-opaque hard-cover (1.0) when the document
            // omits it, matching the core `Cell` default. The compositor honours
            // it in the premultiplied linear-light `over` blend; out-of-range
            // values are rejected by `Layout::validate` downstream.
            opacity: cell.opacity.unwrap_or(1.0),
            // Broadcast per-tile crop/rotation are additive, defaulted fields on
            // the shared `Cell`; this mapper does not yet surface them from the
            // config schema, so spread their defaults.
            ..CoreCell::default()
        })
    }

    /// Reject empty/duplicate source and cell ids.
    fn validate_unique_ids(&self) -> Result<(), ConfigError> {
        let mut seen_sources: HashSet<&str> = HashSet::with_capacity(self.sources.len());
        for source in &self.sources {
            if source.id.is_empty() {
                return Err(ConfigError::Validation(
                    "a source has an empty id".to_owned(),
                ));
            }
            if !seen_sources.insert(source.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate source id {:?}",
                    source.id
                )));
            }
        }
        let mut seen_cells: HashSet<&str> = HashSet::with_capacity(self.cells.len());
        for cell in &self.cells {
            if cell.id.is_empty() {
                return Err(ConfigError::Validation("a cell has an empty id".to_owned()));
            }
            if !seen_cells.insert(cell.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate cell id {:?}",
                    cell.id
                )));
            }
        }
        Ok(())
    }

    /// Ensure every `cells.source.input_id` resolves to a declared source.
    fn validate_cell_bindings(&self) -> Result<(), ConfigError> {
        let source_ids: HashSet<&str> = self.sources.iter().map(|s| s.id.as_str()).collect();
        for cell in &self.cells {
            if let Some(input_id) = &cell.source.input_id {
                if !source_ids.contains(input_id.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "cell {:?} binds unknown source input_id {input_id:?}",
                        cell.id
                    )));
                }
            } else if cell.source.kind.is_none() {
                // Neither a managed reference nor an inline spec: nothing to render.
                return Err(ConfigError::Validation(format!(
                    "cell {:?} has no source binding (neither input_id nor inline kind)",
                    cell.id
                )));
            }
        }
        Ok(())
    }

    /// Ensure outputs are sane (at least one, each with a non-empty codec where
    /// a codec applies).
    fn validate_outputs(&self) -> Result<(), ConfigError> {
        if self.outputs.is_empty() {
            return Err(ConfigError::Validation(
                "at least one output must be declared".to_owned(),
            ));
        }
        for output in &self.outputs {
            let codec = match output {
                Output::RtspServer { codec, .. }
                | Output::LlHls { codec, .. }
                | Output::Hls { codec, .. }
                | Output::Rtmp { codec, .. }
                | Output::Srt { codec, .. } => Some(codec),
                Output::Ndi { .. } => None,
            };
            if let Some(codec) = codec {
                if codec.is_empty() {
                    return Err(ConfigError::Validation(
                        "an output declares an empty codec".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The set of declared cell ids, for reference validation.
    fn cell_ids(&self) -> HashSet<&str> {
        self.cells.iter().map(|c| c.id.as_str()).collect()
    }

    /// Validate probes: each is internally consistent, ids are unique, and every
    /// watched cell resolves to a declared cell.
    fn validate_probes(&self) -> Result<(), ConfigError> {
        let cells = self.cell_ids();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.probes.len());
        for probe in &self.probes {
            probe.validate()?;
            if !seen.insert(probe.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate probe id {:?}",
                    probe.id
                )));
            }
            if !cells.contains(probe.cell.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "probe {:?} watches unknown cell {:?}",
                    probe.id, probe.cell
                )));
            }
        }
        Ok(())
    }

    /// Validate tally profiles: each is internally consistent, ids are unique,
    /// and every index→cell mapping resolves to a declared cell.
    fn validate_tally_profiles(&self) -> Result<(), ConfigError> {
        let cells = self.cell_ids();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.tally_profiles.len());
        for profile in &self.tally_profiles {
            profile.validate()?;
            if !seen.insert(profile.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate tally profile id {:?}",
                    profile.id
                )));
            }
            for rule in &profile.index_cells {
                if !cells.contains(rule.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "tally profile {:?} maps index {} to unknown cell {:?}",
                        profile.id, rule.index, rule.cell
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate salvos: each is internally consistent, ids are unique, and every
    /// referenced cell/source resolves.
    fn validate_salvos(&self) -> Result<(), ConfigError> {
        let cells = self.cell_ids();
        let sources: HashSet<&str> = self.sources.iter().map(|s| s.id.as_str()).collect();
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.salvos.len());
        for salvo in &self.salvos {
            salvo.validate()?;
            if !seen.insert(salvo.id.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate salvo id {:?}",
                    salvo.id
                )));
            }
            for recall in &salvo.sources {
                if !cells.contains(recall.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} rebinds unknown cell {:?}",
                        salvo.id, recall.cell
                    )));
                }
                if !sources.contains(recall.input_id.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} binds unknown source {:?}",
                        salvo.id, recall.input_id
                    )));
                }
            }
            for recall in &salvo.tally {
                if !cells.contains(recall.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} sets tally on unknown cell {:?}",
                        salvo.id, recall.cell
                    )));
                }
            }
            for recall in &salvo.umd {
                if !cells.contains(recall.cell.as_str()) {
                    return Err(ConfigError::Validation(format!(
                        "salvo {:?} sets umd on unknown cell {:?}",
                        salvo.id, recall.cell
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate video walls: ids unique across walls, and each passes the core
    /// wall invariants.
    fn validate_walls(&self) -> Result<(), ConfigError> {
        let mut seen: HashSet<&str> = HashSet::with_capacity(self.walls.len());
        for wall in &self.walls {
            if !seen.insert(wall.name.as_str()) {
                return Err(ConfigError::Validation(format!(
                    "duplicate wall name {:?}",
                    wall.name
                )));
            }
            wall.validate()?;
        }
        Ok(())
    }
}

/// Map a schema fit token onto the core [`FitMode`] (defaulting to `Contain`).
///
/// The schema vocabulary (`fill`/`contain`/`cover`/`none`/`scale_down`) is the
/// CSS `object-fit` set; the core model carries the three blend-relevant modes,
/// so `none`/`scale_down` (which letterbox without stretching) map to
/// [`FitMode::Contain`].
fn parse_fit(fit: Option<&str>) -> FitMode {
    match fit {
        Some("cover") => FitMode::Cover,
        Some("fill") => FitMode::Fill,
        _ => FitMode::Contain,
    }
}
