//! Content-aware fault-probe configuration (config-as-code).
//!
//! These types declare, per tile, the QC probes the monitoring/alarm engine
//! (broadcast-multiviewer brief §4) runs on decoded essence: black, freeze,
//! silence, and loudness-violation, each with a detection **zone**, a level
//! **threshold**, and **dwell** windows (up/down) so a transient blip does not
//! raise (or clear) an alarm. The actual X.733 state machine lives in
//! `mosaic-engine` in a later wave; this crate only owns the *declarative shape*
//! and its validation.
//!
//! All unions are **internally tagged** by `kind` (`#[serde(tag = "kind")]`),
//! never `untagged` (ADR-0010).

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// A normalized detection zone within a tile's picture (`0.0..=1.0` on both
/// axes).
///
/// Video probes (black/freeze) analyse only this sub-rectangle of the tile so a
/// static logo bug or a lower-third does not mask a black/frozen background. A
/// full-frame zone is `{ x: 0, y: 0, w: 1, h: 1 }`, which is the [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DetectionZone {
    /// Left edge (fraction of tile width).
    pub x: f32,
    /// Top edge (fraction of tile height).
    pub y: f32,
    /// Width (fraction of tile width).
    pub w: f32,
    /// Height (fraction of tile height).
    pub h: f32,
}

impl Default for DetectionZone {
    /// The full-frame zone (`x = 0`, `y = 0`, `w = 1`, `h = 1`).
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        }
    }
}

impl DetectionZone {
    /// Validate that the zone is finite, within the unit square, and has
    /// positive extent.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] naming the offending field when the
    /// zone is non-finite, has non-positive width/height, has a negative origin,
    /// or extends beyond `1.0` on either axis.
    pub fn validate(&self, probe_id: &str) -> Result<(), ConfigError> {
        for (name, value) in [("x", self.x), ("y", self.y), ("w", self.w), ("h", self.h)] {
            if !value.is_finite() {
                return Err(ConfigError::Validation(format!(
                    "probe {probe_id:?}: zone.{name} must be finite (got {value})"
                )));
            }
        }
        if self.w <= 0.0 || self.h <= 0.0 {
            return Err(ConfigError::Validation(format!(
                "probe {probe_id:?}: zone must have positive extent (got w={}, h={})",
                self.w, self.h
            )));
        }
        if self.x < 0.0 || self.y < 0.0 {
            return Err(ConfigError::Validation(format!(
                "probe {probe_id:?}: zone origin ({}, {}) must be within 0.0..=1.0",
                self.x, self.y
            )));
        }
        if self.x + self.w > 1.0 || self.y + self.h > 1.0 {
            return Err(ConfigError::Validation(format!(
                "probe {probe_id:?}: zone extends beyond the unit square (x+w={}, y+h={})",
                self.x + self.w,
                self.y + self.h
            )));
        }
        Ok(())
    }
}

/// Dwell windows that debounce a probe: the condition must persist for `up_ms`
/// before the alarm raises, and clear for `down_ms` before it clears.
///
/// Asymmetric dwell (a long `up_ms`, a short `down_ms`, or vice-versa) is the
/// hysteresis that stops an alarm flapping on a marginal signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct Dwell {
    /// Milliseconds the condition must persist before the alarm **raises**.
    pub up_ms: u32,
    /// Milliseconds the condition must clear before the alarm **clears**.
    pub down_ms: u32,
}

impl Default for Dwell {
    /// A symmetric one-second dwell up and down.
    fn default() -> Self {
        Self {
            up_ms: 1000,
            down_ms: 1000,
        }
    }
}

/// The loudness compliance target a [`ProbeKind::Loudness`] probe checks
/// against, internally tagged by `kind`.
///
/// `r128` is EBU R128 (−23 LUFS); `a85` is ATSC A/85 (−24 LKFS). Both carry an
/// explicit integrated-loudness target and a max true-peak ceiling so an
/// operator can override the standard default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LoudnessTarget {
    /// EBU R128 (default integrated target −23.0 LUFS).
    R128 {
        /// Integrated-loudness target in LUFS (e.g. `-23.0`).
        target_lufs: f32,
        /// Maximum permitted true-peak in dBTP (e.g. `-1.0`).
        max_true_peak_dbtp: f32,
    },
    /// ATSC A/85 (default integrated target −24.0 LKFS).
    A85 {
        /// Integrated-loudness target in LKFS (e.g. `-24.0`).
        target_lkfs: f32,
        /// Maximum permitted true-peak in dBTP (e.g. `-2.0`).
        max_true_peak_dbtp: f32,
    },
}

impl LoudnessTarget {
    /// The integrated-loudness target (LUFS/LKFS — the same units).
    #[must_use]
    pub const fn target(&self) -> f32 {
        match self {
            Self::R128 { target_lufs, .. } => *target_lufs,
            Self::A85 { target_lkfs, .. } => *target_lkfs,
        }
    }

    /// The maximum permitted true-peak ceiling in dBTP.
    #[must_use]
    pub const fn max_true_peak_dbtp(&self) -> f32 {
        match self {
            Self::R128 {
                max_true_peak_dbtp, ..
            }
            | Self::A85 {
                max_true_peak_dbtp, ..
            } => *max_true_peak_dbtp,
        }
    }
}

/// The kind-specific parameters of a probe, internally tagged by `kind`.
///
/// Each variant maps to a [`mosaic_core::alarm::AlarmKind`] the engine raises:
/// [`Black`](ProbeKind::Black) → `Black`, [`Freeze`](ProbeKind::Freeze) →
/// `Freeze`, [`Silence`](ProbeKind::Silence) → `Silence`,
/// [`Loudness`](ProbeKind::Loudness) → `LoudnessViolation`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ProbeKind {
    /// Black-picture detection: the picture luma within the zone stays at or
    /// below `luma_threshold` (0–255 on the 8-bit scale) for the dwell window.
    Black {
        /// Luma ceiling (8-bit, `0..=255`) at or below which a pixel is "black".
        luma_threshold: u8,
        /// Detection zone within the tile.
        #[serde(default)]
        zone: DetectionZone,
    },
    /// Freeze detection: successive frames within the zone differ by less than
    /// `difference_threshold` (a per-mille of full-scale luma) for the dwell.
    Freeze {
        /// Inter-frame difference floor (per-mille, `0..=1000`) below which the
        /// picture counts as frozen.
        difference_threshold: u16,
        /// Detection zone within the tile.
        #[serde(default)]
        zone: DetectionZone,
    },
    /// Silence detection: the audio level stays at or below `level_dbfs`
    /// (negative dBFS, e.g. `-60.0`) for the dwell window.
    Silence {
        /// Level ceiling in dBFS at or below which audio counts as silent.
        level_dbfs: f32,
    },
    /// Loudness-violation detection against a compliance target.
    Loudness {
        /// The loudness compliance target.
        target: LoudnessTarget,
    },
}

impl ProbeKind {
    /// The [`mosaic_core::alarm::AlarmKind`] this probe raises.
    #[must_use]
    pub const fn alarm_kind(&self) -> mosaic_core::alarm::AlarmKind {
        match self {
            Self::Black { .. } => mosaic_core::alarm::AlarmKind::Black,
            Self::Freeze { .. } => mosaic_core::alarm::AlarmKind::Freeze,
            Self::Silence { .. } => mosaic_core::alarm::AlarmKind::Silence,
            Self::Loudness { .. } => mosaic_core::alarm::AlarmKind::LoudnessViolation,
        }
    }

    /// Validate the kind-specific parameters (zone geometry, sane thresholds).
    fn validate(&self, probe_id: &str) -> Result<(), ConfigError> {
        match self {
            Self::Black { zone, .. } | Self::Freeze { zone, .. } => zone.validate(probe_id),
            Self::Silence { level_dbfs } => {
                if !level_dbfs.is_finite() {
                    return Err(ConfigError::Validation(format!(
                        "probe {probe_id:?}: silence level_dbfs must be finite (got {level_dbfs})"
                    )));
                }
                Ok(())
            }
            Self::Loudness { target } => {
                if !target.target().is_finite() || !target.max_true_peak_dbtp().is_finite() {
                    return Err(ConfigError::Validation(format!(
                        "probe {probe_id:?}: loudness target/true-peak must be finite"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// One declared probe: a stable id, the tile it watches, its kind-specific
/// parameters, dwell windows, and the X.733 severity it asserts.
///
/// The `severity` is the [`mosaic_core::alarm::PerceivedSeverity`] the engine
/// will stamp on the [`mosaic_core::alarm::AlarmRecord`] this probe raises; it
/// is carried here so the *policy* (how serious a black tile is) is authored in
/// config, not hard-coded in the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Probe {
    /// Stable probe id (unique within the document; referenced by alarms).
    pub id: String,
    /// The cell id this probe watches.
    pub cell: String,
    /// Kind-specific parameters (flattened so `kind` sits at top level).
    #[serde(flatten)]
    pub kind: ProbeKind,
    /// Dwell windows (raise/clear debounce).
    #[serde(default)]
    pub dwell: Dwell,
    /// The perceived severity (X.733) asserted when this probe fires.
    #[serde(default)]
    pub severity: mosaic_core::alarm::PerceivedSeverity,
    /// Whether the alarm latches (held until explicitly reset).
    #[serde(default)]
    pub latched: bool,
}

impl Probe {
    /// Validate this probe's geometry and thresholds in isolation.
    ///
    /// Cell-reference resolution is the document's responsibility (it needs the
    /// cell set) and is enforced by [`crate::MosaicConfig::validate`].
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] for an empty id, an empty cell
    /// reference, an out-of-range detection zone, or a non-finite threshold.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.id.is_empty() {
            return Err(ConfigError::Validation(
                "a probe has an empty id".to_owned(),
            ));
        }
        if self.cell.is_empty() {
            return Err(ConfigError::Validation(format!(
                "probe {:?} has an empty cell reference",
                self.id
            )));
        }
        self.kind.validate(&self.id)
    }
}
