//! Live EBU R128 / ITU-R BS.1770 loudness **normalisation** for the program bus
//! (AUD-6), built on the existing [`LoudnessMeter`](crate::loudness::LoudnessMeter)
//! measurement chain — the meter is **not** reimplemented here.
//!
//! Per ADR-R005/R006 and resilience-and-av §4.1, the program bus is normalised
//! toward a target loudness (`-23` LUFS broadcast / `-16` LUFS streaming) with a
//! true-peak ceiling, while **discrete tracks stay unaltered** (the authenticity
//! guarantee). This is the live, single-pass/dynamic `loudnorm`: it drives a
//! smoothed makeup gain from the running **short-term** (3 s) loudness, with a
//! `-70` LUFS absolute gate so a silenced/lost input is never amplified toward
//! the target, and a true-peak limiter that caps the gain so normalisation
//! **never clips** beyond the ceiling. Live tolerance is `±1 LU` (file-mode
//! `±0.2 LU` is unreachable single-pass — brief §4.1).
//!
//! ## Off the hot path, bounded, deterministic
//! [`LoudnormProcessor::process`] runs on the program-audio bus (the bake
//! consumer thread, *not* the engine output-clock loop), so it cannot stall the
//! output clock (invariant #1) and back-pressures nothing (invariant #10). Per
//! tick it does `O(block)` work (the meter's per-sample accumulate plus one
//! per-sample gain apply) and `O(1)` scalar gain math; the meter's sub-block
//! history is bounded to the short-term window via
//! [`LoudnessMeter::retain_recent`](crate::loudness::LoudnessMeter::retain_recent)
//! so memory never grows with run length. The gain moves by a bounded per-tick
//! step (a one-pole smoother), never instantaneously, so there is no click.

use crate::error::Result;
use crate::format::{AudioBlock, AudioFormat};
use crate::loudness::LoudnessMeter;
use crate::truepeak::TruePeakDetector;

/// The default true-peak ceiling, in dBTP (resilience-and-av §4.1: `-1.5 dBTP`).
/// The emitted program bus is guaranteed not to exceed this by more than the
/// meter's own true-peak estimation error.
pub const DEFAULT_TRUE_PEAK_CEILING_DBTP: f64 = -1.5;

/// The live convergence tolerance, in LU (resilience-and-av §4.1: `±1 LU` live,
/// vs the `±0.2 LU` file-mode that single-pass live normalisation cannot match).
pub const LIVE_TOLERANCE_LU: f64 = 1.0;

/// Number of short-term (3 s) windows of sub-block history to keep. The meter
/// samples short-term loudness from the last 30 sub-blocks (3 s @ 100 ms); we
/// retain a small multiple so short-term/momentary stay continuous while memory
/// stays bounded regardless of run length.
const RETAIN_SUBBLOCKS: usize = 64;

/// The target program loudness for normalisation.
///
/// `-23` LUFS is the EBU R128 / ITU-R BS.1770 broadcast target; `-16` LUFS is the
/// common streaming/web target. [`Custom`](Self::Custom) carries an explicit LUFS
/// value for any other compliance regime.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum LoudnessTarget {
    /// EBU R128 broadcast target, `-23` LUFS.
    Broadcast,
    /// Common streaming/web target, `-16` LUFS.
    Streaming,
    /// An explicit target loudness in LUFS.
    Custom(f64),
}

impl LoudnessTarget {
    /// The target loudness in LUFS.
    #[must_use]
    pub const fn lufs(self) -> f64 {
        match self {
            Self::Broadcast => -23.0,
            Self::Streaming => -16.0,
            Self::Custom(lufs) => lufs,
        }
    }
}

/// Hard-limit a mixed `f64` sample to the `[-1.0, 1.0]` `f32` sample domain — the
/// same belt-and-braces clamp the mixer uses. The applied gain is already capped
/// by the true-peak limiter so this only ever fires on the meter's estimation
/// error margin.
#[allow(clippy::as_conversions, clippy::cast_possible_truncation)] // reason: value clamped to [-1,1]; f64->f32 narrowing is exact-enough and bounded.
fn clamp_sample(v: f64) -> f32 {
    v.clamp(-1.0, 1.0) as f32
}

/// A live program-bus loudness normaliser.
///
/// Build one per run at the program-bus [`AudioFormat`] with
/// [`new`](Self::new), then call [`process`](Self::process) on each program-bus
/// [`AudioBlock`] (between the mixer's `mix_program` and the encoder). Discrete
/// tracks must **not** go through this — use
/// [`discrete_passthrough`](Self::discrete_passthrough) for the identity contract.
#[derive(Debug)]
pub struct LoudnormProcessor {
    format: AudioFormat,
    target_lufs: f64,
    ceiling_dbtp: f64,
    /// The meter measuring the program bus. It carries the oversampled true-peak
    /// detector (this is the one track that displays/limits dBTP, per ADR-R006),
    /// so true-peak runs only here. It measures the **pre-gain** mixed bus; the
    /// makeup gain `target - measured` then brings the emitted bus to target.
    meter: LoudnessMeter,
    /// Per-channel feedforward true-peak limiter detectors run over the **gained**
    /// (about-to-be-emitted) samples. The detectors persist across blocks so the
    /// inter-sample peak is continuous at block seams; they are what guarantees
    /// the emitted true-peak never exceeds the ceiling even on a transient burst
    /// the loudness smoother has not yet reacted to.
    limiters: Vec<TruePeakDetector>,
    /// The current applied makeup gain, in dB (the smoother's state). A bounded
    /// one-pole filter moves it toward the gate-clamped target each tick.
    gain_db: f64,
    /// Per-tick smoothing coefficient in `(0, 1]`: the fraction of the gap to the
    /// instantaneous target the gain closes each block (a one-pole step). Smaller
    /// is smoother/slower; `1.0` would jump instantly.
    smoothing: f64,
}

impl LoudnormProcessor {
    /// The maximum makeup gain the normaliser will ever request, in dB. A live
    /// program bus that is extremely quiet (but above the gate) must not be
    /// boosted without limit — that would amplify noise/hiss toward the target.
    /// The true-peak limiter further caps this per tick.
    const MAX_GAIN_DB: f64 = 24.0;
    /// The maximum attenuation the normaliser will request, in dB (symmetric
    /// bound so a very loud bus is pulled down but the gain state stays bounded).
    const MIN_GAIN_DB: f64 = -24.0;

    /// Build a normaliser for `format` targeting `target`, using the default
    /// `-1.5 dBTP` true-peak ceiling.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`](crate::error::AudioError::InvalidFormat)
    /// if the format has a zero sample rate or zero channels.
    pub fn new(format: AudioFormat, target: LoudnessTarget) -> Result<Self> {
        Self::with_ceiling(format, target, DEFAULT_TRUE_PEAK_CEILING_DBTP)
    }

    /// Build a normaliser with an explicit true-peak `ceiling_dbtp`.
    ///
    /// # Errors
    ///
    /// Returns [`AudioError::InvalidFormat`](crate::error::AudioError::InvalidFormat)
    /// if the format is unusable.
    pub fn with_ceiling(
        format: AudioFormat,
        target: LoudnessTarget,
        ceiling_dbtp: f64,
    ) -> Result<Self> {
        let meter = LoudnessMeter::new(format)?;
        let limiters = (0..format.channel_count())
            .map(|_| TruePeakDetector::new())
            .collect();
        Ok(Self {
            format,
            target_lufs: target.lufs(),
            ceiling_dbtp,
            meter,
            limiters,
            gain_db: 0.0,
            // ~0.15 closes most of the gap over ~1 s of 25 fps ticks — fast
            // enough to converge within seconds, slow enough to be click-free.
            smoothing: 0.15,
        })
    }

    /// The program-bus format this normaliser operates on.
    #[must_use]
    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    /// The target loudness in LUFS.
    #[must_use]
    pub const fn target_lufs(&self) -> f64 {
        self.target_lufs
    }

    /// The true-peak ceiling in dBTP.
    #[must_use]
    pub const fn ceiling_dbtp(&self) -> f64 {
        self.ceiling_dbtp
    }

    /// The current applied makeup gain, in dB (the smoother's state). Useful for
    /// telemetry and for asserting the gate keeps silence from driving the gain.
    #[must_use]
    pub const fn current_gain_db(&self) -> f64 {
        self.gain_db
    }

    /// The discrete-track identity: discrete per-input tracks are carried
    /// **unaltered** (ADR-R005 authenticity guarantee), so the "passthrough" is
    /// simply a clone of the input block. Provided as an associated function so a
    /// caller cannot accidentally route a discrete track through the program
    /// [`process`](Self::process) gain path.
    #[must_use]
    pub fn discrete_passthrough(block: &AudioBlock) -> AudioBlock {
        block.clone()
    }

    /// Normalise one program-bus block toward the target loudness and return the
    /// gain-applied, true-peak-limited block of **exactly the same shape**.
    ///
    /// Steps, all bounded and deterministic:
    /// 1. Feed the block to the meter (the BS.1770 chain) and bound the meter's
    ///    history to the short-term window so memory never grows with run length.
    /// 2. Read the running **short-term** loudness; if it is `None` (below the
    ///    `-70` LUFS absolute gate, e.g. a silenced/lost input) **relax the gain
    ///    toward 0 dB** rather than chasing the target — the gate excludes silence
    ///    (brief §4.1).
    /// 3. Compute the instantaneous makeup gain `target - measured`, clamped to
    ///    `[MIN_GAIN_DB, MAX_GAIN_DB]`, then one-pole-smooth the applied gain
    ///    toward it (no click).
    /// 4. Apply the smoothed gain per sample, then run a **feedforward true-peak
    ///    limiter** over the gained samples: probe their oversampled true-peak and,
    ///    if it would exceed the ceiling, attenuate the whole block by exactly the
    ///    excess so the emitted true-peak lands on (never above) the ceiling — the
    ///    limiter catches transients the loudness smoother has not reacted to, so
    ///    normalisation **never clips**. A final hard clamp to `[-1, 1]` is the
    ///    belt-and-braces guard on the meter's estimation margin.
    pub fn process(&mut self, block: AudioBlock) -> AudioBlock {
        // (1) Measure the pre-gain bus loudness (the makeup gain solves it to
        // target). A (impossible, the format is fixed) ragged push is ignored so a
        // meter hiccup can never stall the bus.
        let _ = self.meter.push_interleaved(block.interleaved());
        self.meter.retain_recent(RETAIN_SUBBLOCKS);

        // (2)/(3) Drive the gain off the running short-term loudness. Gated
        // silence (None) means: do not chase the target — relax toward unity gain.
        let target_gain_db = match self.meter.short_term() {
            Some(measured) => {
                (self.target_lufs - measured).clamp(Self::MIN_GAIN_DB, Self::MAX_GAIN_DB)
            }
            None => 0.0,
        };
        self.gain_db += (target_gain_db - self.gain_db) * self.smoothing;
        let gain_lin = 10f64.powf(self.gain_db / 20.0);

        // (4) Apply the makeup gain, then the feedforward true-peak limiter.
        let channels = self.format.channel_count();
        let gained: Vec<f64> = block
            .interleaved()
            .iter()
            .map(|&s| f64::from(s) * gain_lin)
            .collect();
        let limit_lin = self.true_peak_limit(&gained, channels);
        let out: Vec<f32> = gained
            .iter()
            .map(|&v| clamp_sample(v * limit_lin))
            .collect();

        // The sample count is unchanged, so reconstruction cannot fail; on the
        // (impossible) error degrade to the untouched input rather than panicking
        // on the audio bus (invariant #1: the bus is never short or absent).
        AudioBlock::from_interleaved(self.format, out).unwrap_or(block)
    }

    /// Compute the linear attenuation (`<= 1.0`) to apply to the already-gained
    /// interleaved samples so the emitted true-peak does not exceed the ceiling.
    ///
    /// A clone of each channel's persistent [`TruePeakDetector`] — with its
    /// running peak reset so it reports *this block's* inter-sample peak, not the
    /// all-time maximum — is advanced over this block's gained samples. The FIR
    /// ring is preserved by the clone so the interpolation is continuous across
    /// the block seam (no artificial start-up step). If the block peak exceeds the
    /// ceiling the whole block is scaled by `ceiling / peak` so the loudest
    /// inter-sample point lands on the ceiling. The **persistent** detectors are
    /// then advanced over the FINAL (post-attenuation) samples and their peak
    /// reset, so the next block's probe continues from the right history with a
    /// fresh per-block peak. This is feedforward (look-ahead over the current block
    /// only), so it catches a transient the loudness smoother has not yet seen —
    /// the guarantee that normalisation never clips.
    fn true_peak_limit(&mut self, gained: &[f64], channels: usize) -> f64 {
        if channels == 0 {
            return 1.0;
        }
        let ceiling_lin = 10f64.powf(self.ceiling_dbtp / 20.0);

        // Probe THIS block's true-peak on clones (so the un-attenuated peak never
        // pollutes the persistent state). Reset each clone's running peak so it
        // measures only this block, keeping the FIR ring for seam continuity.
        let mut probe = self.limiters.clone();
        let mut peak = 0.0f64;
        for det in &mut probe {
            det.reset_peak();
        }
        for frame in gained.chunks_exact(channels) {
            for (c, &v) in frame.iter().enumerate() {
                if let Some(det) = probe.get_mut(c) {
                    det.push(v);
                    let p = det.peak_linear();
                    if p > peak {
                        peak = p;
                    }
                }
            }
        }

        let atten = if peak > ceiling_lin && peak > 0.0 {
            ceiling_lin / peak
        } else {
            1.0
        };

        // Advance the PERSISTENT detectors over the final emitted samples so the
        // inter-sample peak is continuous across the next block's seam, then reset
        // their running peak so each block is probed independently.
        for frame in gained.chunks_exact(channels) {
            for (c, &v) in frame.iter().enumerate() {
                if let Some(det) = self.limiters.get_mut(c) {
                    det.push(v * atten);
                }
            }
        }
        for det in &mut self.limiters {
            det.reset_peak();
        }
        atten
    }
}
