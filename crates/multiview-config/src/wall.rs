//! Multi-head video-wall configuration (config-as-code).
//!
//! A [`WallConfig`] declares an independent-per-head output wall
//! (broadcast-multiviewer brief §1, §7): a `cols × rows` grid of [`HeadConfig`]s,
//! each with its own resolution, cadence, orientation, and the **name** of the
//! layout drawn on it, plus bezel compensation for the physical seams. This
//! crate owns the authored shape; [`WallConfig::to_core`] lowers it into the
//! validated [`multiview_core::layout::VideoWall`] the engine consumes.
//!
//! Per-head cadence is an exact rational string (`"num/den"`) — never a float
//! (invariant #3) — reusing the same [`crate::schema::Fps`] type as the canvas.

use multiview_core::layout::{
    BezelCompensation, Canvas as CoreCanvas, Head as CoreHead, Orientation, VideoWall as CoreWall,
};
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;
use crate::schema::Fps;

/// Per-head output geometry and the layout name rendered on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct HeadConfig {
    /// Stable head id, unique within the wall.
    pub id: String,
    /// Head width in pixels.
    pub width: u32,
    /// Head height in pixels.
    pub height: u32,
    /// Head output cadence as an exact rational (`"num/den"` string).
    pub fps: Fps,
    /// Output orientation (landscape/portrait). Defaults to landscape.
    #[serde(default)]
    pub orientation: Orientation,
    /// Name of the layout rendered on this head.
    pub layout: String,
}

impl HeadConfig {
    /// Lower this head into a [`multiview_core::layout::Head`].
    fn to_core(&self) -> CoreHead {
        let cadence = self.fps.rational();
        CoreHead {
            id: self.id.clone(),
            canvas: CoreCanvas {
                width: self.width,
                height: self.height,
                fps_num: cadence.num,
                fps_den: cadence.den,
            },
            orientation: self.orientation,
            layout: self.layout.clone(),
        }
    }
}

/// Bezel compensation between adjacent heads, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WallBezel {
    /// Horizontal gap (px) between horizontally adjacent heads.
    #[serde(default)]
    pub horizontal_px: i32,
    /// Vertical gap (px) between vertically adjacent heads.
    #[serde(default)]
    pub vertical_px: i32,
}

/// A multi-head video wall: a `cols × rows` grid of heads with bezel
/// compensation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct WallConfig {
    /// Wall name.
    pub name: String,
    /// Number of head columns (`> 0`).
    pub cols: u32,
    /// Number of head rows (`> 0`).
    pub rows: u32,
    /// Bezel compensation between adjacent heads.
    #[serde(default)]
    pub bezel: WallBezel,
    /// The heads, in row-major order; exactly `cols * rows` of them.
    pub heads: Vec<HeadConfig>,
}

impl WallConfig {
    /// Lower this wall into a [`multiview_core::layout::VideoWall`] (unvalidated
    /// shape; call [`multiview_core::layout::VideoWall::validate`] or
    /// [`WallConfig::validate`]).
    #[must_use]
    pub fn to_core(&self) -> CoreWall {
        CoreWall {
            name: self.name.clone(),
            cols: self.cols,
            rows: self.rows,
            bezel: BezelCompensation {
                horizontal_px: self.bezel.horizontal_px,
                vertical_px: self.bezel.vertical_px,
            },
            heads: self.heads.iter().map(HeadConfig::to_core).collect(),
        }
    }

    /// Validate this wall by lowering to core and running the core wall
    /// invariants (positive grid, exact head count, unique ids, valid heads,
    /// non-negative bezel).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] wrapping the core failure: a zero
    /// grid dimension, a head-count mismatch, a duplicate head id, an invalid
    /// head canvas/layout name, or negative bezel compensation.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.to_core().validate().map_err(|e| match e {
            multiview_core::Error::Config(msg) => ConfigError::Validation(msg),
            other => ConfigError::Validation(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{HeadConfig, WallConfig};

    /// A well-formed single-head wall (the round-trip baseline these rejection
    /// tests perturb a single field of).
    const GOOD_WALL: &str = r#"
name = "lobby"
cols = 1
rows = 1
[[heads]]
id = "head-l"
width = 1920
height = 1080
fps = "60/1"
layout = "main"
"#;

    #[test]
    fn a_well_formed_wall_parses() {
        let wall: WallConfig = toml::from_str(GOOD_WALL).expect("the baseline wall parses");
        assert_eq!(wall.heads.len(), 1);
        wall.validate().expect("the baseline wall validates");
    }

    #[test]
    fn an_unknown_wall_field_is_rejected_naming_it() {
        // A typo'd top-level wall field (`column` for `cols`) MUST be a loud
        // parse error naming the offender — never a silent revert to a default.
        let doc = GOOD_WALL.replace("cols = 1", "cols = 1\ncolumn = 2");
        let err = toml::from_str::<WallConfig>(&doc)
            .expect_err("an unknown wall field must be rejected");
        let text = err.to_string();
        assert!(
            text.contains("column"),
            "the parse error names the offending field: {text}"
        );
    }

    #[test]
    fn an_unknown_head_field_is_rejected_naming_it() {
        // A typo'd per-head field (`orientaton` for `orientation`) MUST be a
        // loud parse error — otherwise the misspelling silently reverts to the
        // default orientation on a config round-trip.
        let doc = GOOD_WALL.replace("layout = \"main\"", "orientaton = \"portrait\"\nlayout = \"main\"");
        let err = toml::from_str::<WallConfig>(&doc)
            .expect_err("an unknown head field must be rejected");
        let text = err.to_string();
        assert!(
            text.contains("orientaton"),
            "the parse error names the offending head field: {text}"
        );
    }

    #[test]
    fn an_unknown_head_field_at_the_struct_level_is_rejected() {
        // Direct HeadConfig parse: a misspelled field must fail rather than
        // silently dropping (the head struct itself denies unknown fields).
        let doc = r#"
id = "h"
width = 1920
height = 1080
fps = "60/1"
layout = "main"
refresh = "60/1"
"#;
        let err = toml::from_str::<HeadConfig>(doc)
            .expect_err("an unknown HeadConfig field must be rejected");
        assert!(err.to_string().contains("refresh"), "{err}");
    }
}
