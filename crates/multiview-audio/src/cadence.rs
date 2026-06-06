//! The per-output-tick audio sample budget (AUD-3, [`SampleClock`]).
//!
//! The output clock emits one video frame per fixed tick (invariant #1). Audio
//! must ride that same clock: each tick the program bus advances by exactly
//! `sample_rate / fps` samples. For integer-divisible rates that is a constant
//! (25 fps @ 48 kHz = 1920), but for the NTSC `1001`-denominator family it is
//! fractional (30000/1001 @ 48 kHz = 1601.6 samples/tick), so a single tick must
//! emit a whole number of samples while the **long-run** total stays an exact
//! function of the tick count — the audio analogue of invariant #3's "never
//! float fps". [`SampleClock`] does this with pure integer (rational) remainder
//! accumulation: never a float, never an `as` truncation, so the cumulative
//! sample position tracks the ideal real position to within one sample forever
//! and never drifts (ADR-R005 §4.1).

use multiview_core::time::Rational;

/// The per-tick audio sample-budget accumulator.
///
/// Construct one per run with [`SampleClock::new`] from the output sample rate
/// and the exact output cadence (`num/den` fps); call [`SampleClock::next_tick`]
/// once per output tick to get the exact number of samples to mix/emit for that
/// tick. The running total after `t` ticks is exactly
/// `floor(t · sample_rate · den / num)` — gap-free and never ahead of real time.
#[derive(Debug, Clone)]
pub struct SampleClock {
    /// Samples-per-tick numerator increment: `sample_rate · fps.den`.
    step_num: u64,
    /// Samples-per-tick denominator: `fps.num` (the divisor). Always `>= 1`.
    step_den: u64,
    /// Accumulated remainder (in `step_den` units), `0 <= carry < step_den`.
    carry: u64,
}

impl SampleClock {
    /// Build a sample clock for `sample_rate` Hz output paced by `fps`.
    ///
    /// `fps` is the exact output cadence (e.g. `30000/1001`); its denominator is
    /// canonically strictly positive. A degenerate cadence (zero numerator) is
    /// clamped to a divisor of 1 so the clock still advances deterministically
    /// rather than dividing by zero (the caller validates real cadences upstream).
    #[must_use]
    pub fn new(sample_rate: u32, fps: Rational) -> Self {
        let den = fps.den.unsigned_abs();
        let num = fps.num.unsigned_abs().max(1);
        Self {
            step_num: u64::from(sample_rate).saturating_mul(den),
            step_den: num,
            carry: 0,
        }
    }

    /// The number of samples (per channel) to emit for the next output tick.
    ///
    /// Exact rational accumulation: adds `step_num` to the carried remainder,
    /// takes the integer quotient as this tick's budget, and keeps the remainder
    /// for the next tick. Over any run the totals are exact (no float drift).
    pub fn next_tick(&mut self) -> usize {
        // carry < step_den and step_num are both bounded by realistic rates, so
        // the sum cannot overflow u64; saturating_add is belt-and-braces.
        let total = self.carry.saturating_add(self.step_num);
        let samples = total / self.step_den;
        self.carry = total % self.step_den;
        usize::try_from(samples).unwrap_or(usize::MAX)
    }
}
