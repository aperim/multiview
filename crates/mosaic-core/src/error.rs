//! Error taxonomy for the workspace.
//!
//! Every fallible operation in `mosaic-core` returns [`Result`], whose error
//! arm is the workspace-wide [`enum@Error`] enum. Downstream crates may define their
//! own `thiserror` enums and convert into this taxonomy at their boundary.
use thiserror::Error;

/// Convenient result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error type spanning the Mosaic pipeline stages.
///
/// Marked `#[non_exhaustive]`: downstream `match` statements must include a
/// wildcard arm so new variants can be added without a breaking change.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// An ingest/source failure.
    #[error("input error: {0}")]
    Input(String),
    /// A decode failure.
    #[error("decode error: {0}")]
    Decode(String),
    /// A compositing failure.
    #[error("compositor error: {0}")]
    Compositor(String),
    /// An encode failure.
    #[error("encode error: {0}")]
    Encode(String),
    /// An output/mux/serve failure.
    #[error("output error: {0}")]
    Output(String),
    /// A configuration or template-validation error.
    #[error("config error: {0}")]
    Config(String),
    /// Functionality not yet implemented in this scaffold.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}
