//! # mosaic-hal
//!
//! Hardware-abstraction layer for the **Mosaic** engine: the pure-Rust
//! capability + cost + planner model that feeds invariants #6
//! (decode-at-display-resolution, budgeted in megapixels/sec) and #9
//! (resource-adaptive degradation).
//!
//! It has four cooperating pieces:
//!
//! - [`capability`] — [`Capability`] descriptors: per `(BackendKind, Stage)`,
//!   the max resolution, accepted pixel formats, and decode-resize support.
//! - [`registry`] — the [`BackendRegistry`]: register/query capabilities by
//!   `(stage, kind)`, seeded with the always-available software tier.
//! - [`cost`] — the [`CostBudget`] and per-tile [`TileLoad`], all in
//!   **megapixels/sec** (never stream count).
//! - [`planner`] — the [`Planner`]: admission control (does a [`Plan`] fit
//!   every engine budget?) plus the hysteresis-controlled, cheapest-impact-first
//!   [`degradation`] ladder (no flapping).
//!
//! Hardware probing ([`probe`]) is the only seam that touches vendors. It
//! follows the three-layer model (core-engine §6.2): the injectable
//! [`probe::DeviceProbe`] decides device *presence* (environment detection, no
//! native SDK), and the feature-gated backend crates later refine the resulting
//! [`Capability`] with true vendor caps queries. The native paths live behind
//! off-by-default features (`cuda`, `vaapi`, `qsv`, `videotoolbox`); the default
//! build is pure Rust with no native deps and every hardware probe reports
//! *unavailable*. With a feature on but no device present (CI), the probe still
//! reports *unavailable* cleanly — never a panic. The library target is
//! `mosaic_hal`.
//!
//! See [core-engine §6](../../docs/research/core-engine.md),
//! [efficiency](../../docs/research/efficiency.md), ADR-0003, and ADR-0004.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod capability;
pub mod cost;
pub mod degradation;
pub mod error;
pub mod planner;
pub mod probe;
pub mod registry;

pub use capability::{Capability, Resolution, Stage};
pub use cost::{CostBudget, TileLoad};
pub use degradation::{
    actions_at_level, DegradationAction, Hysteresis, HysteresisConfig, LadderMove, MAX_LEVEL,
};
pub use error::{Error, Result};
pub use planner::{Admission, Plan, Planner, StageUsage};
pub use probe::{
    detect, software_capability, DeviceCaps, DeviceProbe, EnvProbe, HardwareKind, ProbeOutcome,
    StageSupport,
};
pub use registry::BackendRegistry;

// Re-export the shared enums the HAL describes assignments with, so downstream
// crates can name them through `mosaic_hal` without a direct `mosaic_core`
// dependency where convenient.
pub use mosaic_core::traits::BackendKind;
