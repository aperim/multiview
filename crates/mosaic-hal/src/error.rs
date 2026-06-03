//! Error taxonomy for the hardware-abstraction layer.
//!
//! Every fallible operation in `mosaic-hal` returns [`Result`], whose error arm
//! is the crate-local [`enum@Error`] enum. At the crate boundary these convert into
//! the workspace-wide [`mosaic_core::Error`] taxonomy (a `Config` arm carries
//! the message), so downstream crates see one uniform error surface.
use crate::capability::Stage;
use mosaic_core::traits::BackendKind;
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = core::result::Result<T, Error>;

/// Errors raised by capability detection, the registry, and the planner.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error, Clone, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// A backend was requested for a `(stage, kind)` pair that is not
    /// registered.
    #[error("no backend registered for stage {stage:?} kind {kind:?}")]
    BackendNotFound {
        /// The pipeline stage that was queried.
        stage: Stage,
        /// The backend kind that was queried.
        kind: BackendKind,
    },

    /// A backend with this `(stage, kind)` pair is already registered and
    /// would be overwritten.
    #[error("backend already registered for stage {stage:?} kind {kind:?}")]
    DuplicateBackend {
        /// The pipeline stage of the conflicting registration.
        stage: Stage,
        /// The backend kind of the conflicting registration.
        kind: BackendKind,
    },

    /// A hardware backend was requested but is unavailable on this host
    /// (feature not compiled in, or the probe found no usable device).
    #[error("backend {kind:?} is unavailable: {reason}")]
    BackendUnavailable {
        /// The backend kind that could not be provided.
        kind: BackendKind,
        /// Human-readable reason (e.g. `"cuda feature not enabled"`).
        reason: &'static str,
    },

    /// A proposed plan cannot be admitted because it exceeds an engine budget.
    #[error(
        "admission denied: {stage:?} load {requested_mpps:.3} Mpix/s exceeds \
         budget {budget_mpps:.3} Mpix/s"
    )]
    BudgetExceeded {
        /// The stage whose budget was exceeded.
        stage: Stage,
        /// The requested load, in megapixels per second.
        requested_mpps: f64,
        /// The available budget, in megapixels per second.
        budget_mpps: f64,
    },

    /// A capability descriptor or cost figure was structurally invalid.
    #[error("invalid capability/cost: {0}")]
    InvalidCapability(&'static str),
}

impl From<Error> for mosaic_core::Error {
    fn from(value: Error) -> Self {
        mosaic_core::Error::Config(value.to_string())
    }
}
