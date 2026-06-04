//! The per-crate error taxonomy for `multiview-engine`.
//!
//! Engine-level failures are configuration/startup errors (a bad cadence, an
//! invalid layout, a malformed control-loop budget) — the **hot path itself
//! never returns an error to a caller**: per invariants #1 and #2 the output
//! clock and the drive loop hold the last-good frame (or a `NoSignal` card) and
//! keep ticking rather than propagating a failure outward. These variants
//! therefore describe the *setup* seams, where a `Result` is the right contract.
use multiview_core::time::Rational;
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors raised when configuring or starting the engine.
///
/// `#[non_exhaustive]`: downstream `match` statements must include a wildcard
/// arm so new variants can be added without a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The output cadence is not a usable positive rational (invariant #1/#3:
    /// the clock must have an exact, non-degenerate `num/den` frame rate).
    #[error("invalid output cadence {num}/{den}: must be a positive, non-degenerate rational")]
    InvalidCadence {
        /// The offending numerator.
        num: i64,
        /// The offending denominator.
        den: i64,
    },

    /// The active layout failed structural validation (delegated to
    /// [`multiview_core::layout::Layout::validate`]).
    #[error("invalid layout: {0}")]
    InvalidLayout(String),

    /// The admission/degradation control loop was given a malformed budget or
    /// hysteresis configuration (delegated to `multiview-hal`).
    #[error("invalid control-loop configuration: {0}")]
    InvalidControlLoop(String),

    /// A canvas geometry was rejected by the compositor (e.g. odd dimensions).
    #[error("compositor rejected canvas geometry: {0}")]
    Canvas(String),
}

impl Error {
    /// Construct an [`Error::InvalidCadence`] from a [`Rational`].
    #[must_use]
    pub(crate) const fn invalid_cadence(cadence: Rational) -> Self {
        Self::InvalidCadence {
            num: cadence.num,
            den: cadence.den,
        }
    }
}
