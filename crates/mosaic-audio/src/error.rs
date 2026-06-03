//! Error taxonomy for [`mosaic-audio`](crate).
use thiserror::Error;

/// Result alias for the audio crate.
pub type Result<T> = core::result::Result<T, AudioError>;

/// Errors produced by the audio mix/route model and loudness metering.
///
/// `#[non_exhaustive]` so new arms can be added without a breaking change;
/// downstream `match` statements must carry a wildcard arm.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AudioError {
    /// A PCM block's sample count is not a whole number of frames for its
    /// channel layout (interleaved samples must be a multiple of the channel
    /// count).
    #[error("ragged audio block: {samples} samples is not a multiple of {channels} channels")]
    RaggedBlock {
        /// Number of interleaved samples supplied.
        samples: usize,
        /// Channel count implied by the format.
        channels: usize,
    },

    /// A block submitted to the mixer does not match the mixer's working
    /// format.
    #[error(
        "audio format mismatch: expected {expected_rate} Hz / {expected_channels} ch, \
         got {actual_rate} Hz / {actual_channels} ch"
    )]
    FormatMismatch {
        /// Sample rate the mixer expects.
        expected_rate: u32,
        /// Channel count the mixer expects.
        expected_channels: usize,
        /// Sample rate of the offending block.
        actual_rate: u32,
        /// Channel count of the offending block.
        actual_channels: usize,
    },

    /// A route or submission referenced an input id the mixer does not know.
    #[error("unknown mixer input id: {0}")]
    UnknownInput(usize),

    /// A meter was constructed with an unusable audio format (e.g. zero sample
    /// rate or zero channels).
    #[error("invalid audio format: {0}")]
    InvalidFormat(&'static str),

    /// A real audio decode/resample (behind the `ffmpeg` feature) failed. The
    /// underlying libav error is flattened to a string so this error — and the
    /// whole audio crate's public surface — never names a binding type.
    #[cfg(feature = "ffmpeg")]
    #[error("audio decode failed: {0}")]
    Decode(String),
}

impl From<AudioError> for mosaic_core::Error {
    fn from(value: AudioError) -> Self {
        mosaic_core::Error::Config(value.to_string())
    }
}
