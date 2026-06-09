//! True-peak (inter-sample peak) estimation by oversampling, per ITU-R
//! BS.1770-4 Annex 2.
//!
//! Sample-peak metering underestimates the real analog peak because the highest
//! point of the reconstructed waveform can fall *between* samples. BS.1770
//! prescribes oversampling (4× is sufficient up to 48 kHz) through a low-pass
//! interpolation filter, then taking the maximum absolute value of the
//! upsampled signal as the true-peak estimate (dBTP).
//!
//! This implements a 4× polyphase windowed-sinc interpolator: the prototype
//! low-pass FIR is split into four phase sub-filters, each producing one of the
//! four output samples per input sample.

const OVERSAMPLE: usize = 4;
/// Number of taps per polyphase phase (prototype length = `PHASE_TAPS *
/// OVERSAMPLE`). A 12-tap-per-phase (48-tap prototype) filter comfortably
/// suppresses imaging up to 48 kHz, matching the BS.1770 guidance.
const PHASE_TAPS: usize = 12;

/// A 4× oversampling true-peak detector for one channel.
///
/// Feed samples in order; the detector tracks the running maximum absolute
/// value of the reconstructed (upsampled) waveform.
#[derive(Debug, Clone)]
pub struct TruePeakDetector {
    /// `OVERSAMPLE` phase sub-filters, each `PHASE_TAPS` taps long.
    phases: [[f64; PHASE_TAPS]; OVERSAMPLE],
    /// Ring of the last `PHASE_TAPS` input samples (most recent at `head`).
    history: [f64; PHASE_TAPS],
    head: usize,
    /// Number of samples pushed so far; the filter is primed once this reaches
    /// `PHASE_TAPS` (the ring is full of real input rather than the initial
    /// zeros, so no artificial start-up step is counted as a peak).
    primed: usize,
    peak: f64,
}

impl Default for TruePeakDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl TruePeakDetector {
    /// Construct a detector with a precomputed polyphase interpolation filter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            phases: build_polyphase(),
            history: [0.0; PHASE_TAPS],
            head: 0,
            primed: 0,
            peak: 0.0,
        }
    }

    /// Push one input sample, updating the running true-peak.
    pub fn push(&mut self, x: f64) {
        // Insert newest sample into the ring buffer. `head < PHASE_TAPS` always
        // (it is kept reduced mod `PHASE_TAPS`), so `get_mut` always succeeds;
        // the fallback can never trigger but keeps the code panic-free.
        if let Some(slot) = self.history.get_mut(self.head) {
            *slot = x;
        }
        if self.primed < PHASE_TAPS {
            self.primed += 1;
        }
        // Skip output until the ring holds only real input — otherwise the
        // initial zeros form an artificial step whose filter overshoot would be
        // mis-counted as a true peak.
        if self.primed >= PHASE_TAPS {
            // Evaluate all OVERSAMPLE interpolated outputs for this input step.
            for phase in &self.phases {
                let mut acc = 0.0;
                // tap j multiplies the sample j steps in the past.
                for (j, coeff) in phase.iter().enumerate() {
                    let idx = (self.head + PHASE_TAPS - j) % PHASE_TAPS;
                    acc += coeff * self.history.get(idx).copied().unwrap_or(0.0);
                }
                let mag = acc.abs();
                if mag > self.peak {
                    self.peak = mag;
                }
            }
        }
        self.head = (self.head + 1) % PHASE_TAPS;
    }

    /// The running true-peak magnitude (linear, where 1.0 == 0 dBTP).
    #[must_use]
    pub fn peak_linear(&self) -> f64 {
        self.peak
    }

    /// Reset only the running peak to zero, **keeping** the FIR history and primed
    /// state intact.
    ///
    /// A normal [`peak_linear`](Self::peak_linear) is a running maximum over the
    /// whole signal — it never decays. A feedforward limiter that needs the peak of
    /// *just the most recent span* (e.g. the loudnorm per-block true-peak limiter,
    /// AUD-6) zeroes the peak between spans while preserving the polyphase ring so
    /// the inter-sample interpolation stays continuous across the span boundary
    /// (no artificial seam step).
    pub fn reset_peak(&mut self) {
        self.peak = 0.0;
    }

    /// The running true-peak in dBTP, or `None` if no non-zero sample has been
    /// seen (silence has no defined peak level).
    #[must_use]
    pub fn peak_dbtp(&self) -> Option<f64> {
        if self.peak <= 0.0 {
            None
        } else {
            Some(20.0 * self.peak.log10())
        }
    }
}

/// Exact small-index `usize -> f64` for filter design (values are < 48).
#[allow(clippy::as_conversions, clippy::cast_precision_loss)] // reason: filter-design indices are tiny (< 2^53); lossless, no fallible From.
fn idx_f64(n: usize) -> f64 {
    n as f64
}

/// Build the `OVERSAMPLE` polyphase sub-filters from a Blackman-windowed sinc
/// interpolation prototype.
///
/// The sinc is centred exactly on a phase-0 sample (an integer multiple of
/// `OVERSAMPLE`), so phase 0 is a pure passthrough (it reproduces the original
/// samples) and phases `1..OVERSAMPLE` interpolate between them. Each phase is
/// then normalised to unity DC gain, so a constant input reconstructs to the
/// same constant and the true-peak of a passband tone is not inflated by filter
/// gain.
fn build_polyphase() -> [[f64; PHASE_TAPS]; OVERSAMPLE] {
    let proto_len = PHASE_TAPS * OVERSAMPLE;
    // Centre on a phase-0 sample => phase 0 is an exact passthrough.
    let center = idx_f64((PHASE_TAPS / 2) * OVERSAMPLE);
    let cutoff = 1.0 / idx_f64(OVERSAMPLE); // passband edge = original Nyquist
    let two_pi = 2.0 * core::f64::consts::PI;
    let last = idx_f64(proto_len - 1);
    let mut proto = vec![0.0f64; proto_len];
    for (n, tap) in proto.iter_mut().enumerate() {
        let t = idx_f64(n) - center;
        let x = two_pi * cutoff * t;
        let sinc = if x.abs() < 1e-9 { 1.0 } else { x.sin() / x };
        // Blackman window over the prototype span.
        let r = idx_f64(n) / last;
        let window = 0.42 - 0.5 * (two_pi * r).cos() + 0.08 * (2.0 * two_pi * r).cos();
        *tap = sinc * window;
    }

    // Polyphase decomposition: phase p uses prototype taps p, p+OVERSAMPLE, …
    // Normalise each phase to unity DC gain.
    let mut phases = [[0.0f64; PHASE_TAPS]; OVERSAMPLE];
    for (p, phase) in phases.iter_mut().enumerate() {
        let mut sum = 0.0;
        for (j, tap) in phase.iter_mut().enumerate() {
            let proto_idx = j * OVERSAMPLE + p;
            let coeff = proto.get(proto_idx).copied().unwrap_or(0.0);
            *tap = coeff;
            sum += coeff;
        }
        if sum.abs() > 1e-12 {
            for tap in phase.iter_mut() {
                *tap /= sum;
            }
        }
    }
    phases
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    // reason: loop-index to float is exact for the small ranges used here.
    #![allow(clippy::as_conversions, clippy::cast_precision_loss)]
    use super::*;

    #[test]
    fn dc_passes_at_unity() {
        let mut d = TruePeakDetector::new();
        for _ in 0..200 {
            d.push(0.5);
        }
        approx::assert_abs_diff_eq!(d.peak_linear(), 0.5, epsilon = 0.02);
    }

    #[test]
    fn silence_has_no_dbtp() {
        let mut d = TruePeakDetector::new();
        for _ in 0..100 {
            d.push(0.0);
        }
        assert_eq!(d.peak_dbtp(), None);
    }
}
