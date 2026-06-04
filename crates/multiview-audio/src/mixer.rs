//! The mix/route model: a program bus plus discrete per-input tracks, with a
//! per-input gain/route matrix (ADR-R005).
//!
//! This is the pure-Rust *model* of audio routing — it operates on in-memory
//! [`AudioBlock`]s. The libav decode/resample that fills those blocks lives
//! behind the off-by-default `ffmpeg` feature and is not part of this layer.
//!
//! Routing rules (per the ADR):
//! - Each input fans out to (a) a clean **discrete track** carried unaltered
//!   and (b) the mixed **program bus** scaled by the input's program route
//!   gain.
//! - An input with no fresh block this tick contributes **silence** to the bus
//!   (gap-free; the mixer never stalls waiting for an input).
//! - The program bus is hard-limited to the `[-1.0, 1.0]` sample domain so it
//!   never overflows.
use crate::error::{AudioError, Result};
use crate::format::{AudioBlock, AudioFormat};

/// A handle to a routing endpoint. Currently an input slot; the type leaves
/// room for future endpoints (e.g. named submixes) without changing call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RoutePoint {
    input: usize,
}

impl RoutePoint {
    /// A route point referring to mixer input `index`.
    #[must_use]
    pub const fn input(index: usize) -> Self {
        Self { input: index }
    }

    /// The input index this route point refers to.
    #[must_use]
    pub const fn index(self) -> usize {
        self.input
    }
}

/// One input strip: its identifier, program-route gain, and the latest
/// submitted block (if any this tick).
#[derive(Debug)]
struct InputStrip {
    id: String,
    program_gain: f64,
    routed_to_program: bool,
    latest: Option<AudioBlock>,
}

/// A program-bus + discrete-track mixer over a fixed working [`AudioFormat`].
#[derive(Debug)]
pub struct Mixer {
    format: AudioFormat,
    inputs: Vec<InputStrip>,
}

impl Mixer {
    /// Create a mixer whose program bus runs at `format`.
    #[must_use]
    pub fn new(format: AudioFormat) -> Self {
        Self {
            format,
            inputs: Vec::new(),
        }
    }

    /// The mixer's working format (also the program bus format).
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// Register a new input strip, returning its [`RoutePoint`]. The input is
    /// not routed to the program bus until [`Mixer::route_to_program`] is
    /// called.
    pub fn add_input(&mut self, id: impl Into<String>) -> RoutePoint {
        let index = self.inputs.len();
        self.inputs.push(InputStrip {
            id: id.into(),
            program_gain: 1.0,
            routed_to_program: false,
            latest: None,
        });
        RoutePoint::input(index)
    }

    /// Route an input to the program bus at linear `gain`. Calling again
    /// updates the gain. A no-op (but not an error) for an unknown input — use
    /// only handles returned by [`Mixer::add_input`].
    pub fn route_to_program(&mut self, point: RoutePoint, gain: f64) {
        if let Some(strip) = self.inputs.get_mut(point.index()) {
            strip.program_gain = gain;
            strip.routed_to_program = true;
        }
    }

    /// Remove an input from the program bus (its discrete track remains).
    pub fn unroute_from_program(&mut self, point: RoutePoint) {
        if let Some(strip) = self.inputs.get_mut(point.index()) {
            strip.routed_to_program = false;
        }
    }

    /// Submit the latest decoded block for an input this tick.
    ///
    /// # Errors
    ///
    /// - [`AudioError::UnknownInput`] if `point` is not a known input.
    /// - [`AudioError::FormatMismatch`] if the block's format differs from the
    ///   mixer's working format.
    pub fn submit(&mut self, point: RoutePoint, block: AudioBlock) -> Result<()> {
        if block.format() != self.format {
            return Err(AudioError::FormatMismatch {
                expected_rate: self.format.sample_rate(),
                expected_channels: self.format.channel_count(),
                actual_rate: block.format().sample_rate(),
                actual_channels: block.format().channel_count(),
            });
        }
        let strip = self
            .inputs
            .get_mut(point.index())
            .ok_or(AudioError::UnknownInput(point.index()))?;
        strip.latest = Some(block);
        Ok(())
    }

    /// The discrete (clean, unaltered) track for an input this tick, or `None`
    /// if the input is unknown or has not submitted a block.
    #[must_use]
    pub fn discrete_track(&self, point: RoutePoint) -> Option<&AudioBlock> {
        self.inputs.get(point.index())?.latest.as_ref()
    }

    /// The identifier of an input, if known.
    #[must_use]
    pub fn input_id(&self, point: RoutePoint) -> Option<&str> {
        self.inputs.get(point.index()).map(|s| s.id.as_str())
    }

    /// Mix all program-routed inputs into the program bus for this tick.
    ///
    /// The bus length is the longest submitted block; shorter inputs (and
    /// dropped inputs) contribute silence for the missing frames. The result is
    /// hard-limited to `[-1.0, 1.0]`. Returns a silent block of length 0 when no
    /// input has submitted anything.
    #[must_use]
    pub fn mix_program(&self) -> Option<AudioBlock> {
        let channels = self.format.channel_count();
        if channels == 0 {
            return None;
        }
        // Longest routed-and-submitted block sets the bus length.
        let frames = self
            .inputs
            .iter()
            .filter(|s| s.routed_to_program)
            .filter_map(|s| s.latest.as_ref())
            .map(AudioBlock::frame_count)
            .max()
            .unwrap_or(0);

        let mut acc = vec![0.0f64; frames.saturating_mul(channels)];
        for strip in self.inputs.iter().filter(|s| s.routed_to_program) {
            let Some(block) = strip.latest.as_ref() else {
                continue; // dropout => contributes silence
            };
            let gain = strip.program_gain;
            for (dst, &src) in acc.iter_mut().zip(block.interleaved().iter()) {
                *dst += gain * f64::from(src);
            }
        }

        let samples: Vec<f32> = acc.iter().map(|&v| clamp_sample(v)).collect();
        // Length is `frames * channels` by construction, so this never errors.
        AudioBlock::from_interleaved(self.format, samples).ok()
    }
}

/// Hard-limit a mixed `f64` sample to the `[-1.0, 1.0]` `f32` sample domain.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // reason: value is clamped to [-1,1]; f64->f32 narrowing is exact-enough and bounded.
fn clamp_sample(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}
