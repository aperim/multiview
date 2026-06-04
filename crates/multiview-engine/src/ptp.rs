//! PTP / SMPTE **ST 2059-2** servo **math** — a pure, disciplined-reference-clock
//! estimator (always compiled, unit + property tested).
//!
//! ST 2110-10 mandates the **ST 2059-2 profile of IEEE 1588-2008 (PTP)** as the
//! media-reference timing for uncompressed-over-IP. A PTP receiver exchanges
//! Sync / Follow-Up / Delay-Req / Delay-Resp messages to measure, for each
//! exchange, the **offset** of the local clock from the grandmaster and the
//! **mean path delay**. Those raw measurements are noisy (network jitter,
//! timestamp granularity); a **servo** filters them into a smooth estimate of the
//! grandmaster time and the local frequency error so a disciplined reference can
//! be derived.
//!
//! This module is the **servo math only** — the filtering and convergence logic
//! over injected `(offset, delay)` samples. It is a pure value machine: samples
//! in, a refined offset/frequency estimate out, no sockets, no clocks of its own,
//! no `.await`. The actual hardware PTP NIC / PHC (PTP Hardware Clock) binding
//! lives behind the off-by-default **`ptp`** feature in the `phc` submodule
//! (compiled only with that feature enabled) and is compile-verified only (this
//! environment has no PTP NIC).
//!
//! ## Invariant #1 is preserved: PTP does **not** pace the output clock
//!
//! This is load-bearing. The Multiview output clock
//! ([`OutputClock`](crate::clock::OutputClock)) remains the **single pacing
//! authority**: `out_pts = f(tick)` is computed purely from the integer tick
//! counter and the fixed cadence, exactly as before. The PTP servo disciplines a
//! **separate reference estimate** — used to *timestamp ingest in the
//! grandmaster's timebase* and to align ST 2110 senders/receivers — and it can
//! neither stall nor speed up the tick loop. A drifting, jittering, or absent
//! grandmaster changes only the reference estimate this servo reports; it does
//! **not** change how many frames the output emits, nor when, because the runtime
//! paces on its own monotonic [`TimeSource`](crate::clock::TimeSource), never on
//! the PTP estimate. (If a deployment later chooses to *seed* the output clock's
//! origin from PTP at startup, that is a one-time anchor, not per-tick pacing —
//! the cadence stays the exact rational.)
//!
//! All servo arithmetic is in **integer nanoseconds** (offsets) and **parts per
//! billion** (frequency), never float fps — consistent with invariant #3.

/// One PTP measurement: the instantaneous master↔local **offset** and the
/// measured **mean path delay**, both in nanoseconds.
///
/// Offset is `local_clock - master_clock`: a **positive** offset means the local
/// clock is *ahead* of the grandmaster and must be slowed / stepped back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtpSample {
    /// Measured offset of the local clock from the master, in nanoseconds
    /// (`local - master`).
    pub offset_ns: i64,
    /// Measured mean path delay, in nanoseconds (non-negative).
    pub delay_ns: i64,
}

impl PtpSample {
    /// Construct a sample, clamping a negative path delay to zero (delay is
    /// physically non-negative; a negative measurement is timestamp noise).
    #[must_use]
    pub const fn new(offset_ns: i64, delay_ns: i64) -> Self {
        Self {
            offset_ns,
            delay_ns: if delay_ns < 0 { 0 } else { delay_ns },
        }
    }
}

/// Tuning for the [`PtpServo`].
///
/// The servo is a first-order IIR (exponential) filter on the offset plus a
/// frequency estimate derived from the smoothed-offset trend. The smoothing
/// strength is expressed as an integer reciprocal weight `alpha_recip`: each new
/// sample contributes `1 / alpha_recip` of the correction, so a larger value is
/// heavier smoothing (slower but steadier convergence). A `step_threshold_ns`
/// bounds how far a single sample may pull the estimate: a measurement further
/// off than this is treated as a **step** (e.g. a grandmaster change) and applied
/// in full rather than filtered, mirroring the PTP servo "step vs slew" rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServoConfig {
    /// Reciprocal smoothing weight for the offset filter (`>= 1`). Larger =
    /// heavier smoothing.
    pub alpha_recip: i64,
    /// Offsets whose magnitude exceeds this are applied as a hard step rather
    /// than slewed.
    pub step_threshold_ns: i64,
    /// An outlier guard: samples whose measured delay exceeds this many times the
    /// running median-ish average delay are ignored as path-asymmetry spikes
    /// (expressed as a percentage, e.g. `400` = 4×). Zero disables the guard.
    pub delay_outlier_pct: i64,
}

impl ServoConfig {
    /// A sane default: weight 8 (each sample contributes 1/8 of the correction),
    /// a 1 ms step threshold, and a 4× delay-outlier guard.
    #[must_use]
    pub const fn new_default() -> Self {
        Self {
            alpha_recip: 8,
            step_threshold_ns: 1_000_000,
            delay_outlier_pct: 400,
        }
    }
}

impl Default for ServoConfig {
    fn default() -> Self {
        Self::new_default()
    }
}

/// The PTP/ST 2059-2 servo: filters noisy `(offset, delay)` samples into a
/// smoothed offset estimate and a frequency-error estimate (parts per billion).
///
/// Pure and deterministic: feed it [`PtpSample`]s with [`PtpServo::update`] and
/// read [`PtpServo::offset_ns`] / [`PtpServo::frequency_ppb`]. It owns no clock
/// and performs no I/O.
#[derive(Debug, Clone)]
pub struct PtpServo {
    config: ServoConfig,
    /// The current smoothed offset estimate (ns).
    offset_ns: i64,
    /// The current frequency-error estimate (parts per billion). Positive means
    /// the local clock runs fast relative to the master.
    frequency_ppb: i64,
    /// Running average path delay (ns), for the outlier guard.
    avg_delay_ns: i64,
    /// Whether any sample has been applied yet (the first sample anchors the
    /// estimate exactly).
    locked: bool,
    /// Count of samples accepted (for diagnostics / lock detection).
    accepted: u64,
}

impl PtpServo {
    /// Create a servo with the given tuning. The estimate starts at zero offset
    /// and zero frequency error until the first sample anchors it.
    #[must_use]
    pub fn new(config: ServoConfig) -> Self {
        let alpha_recip = config.alpha_recip.max(1);
        Self {
            config: ServoConfig {
                alpha_recip,
                ..config
            },
            offset_ns: 0,
            frequency_ppb: 0,
            avg_delay_ns: 0,
            locked: false,
            accepted: 0,
        }
    }

    /// The current smoothed offset estimate (`local - master`), in nanoseconds.
    #[must_use]
    pub const fn offset_ns(&self) -> i64 {
        self.offset_ns
    }

    /// The current frequency-error estimate, in parts per billion (positive =
    /// local clock fast).
    #[must_use]
    pub const fn frequency_ppb(&self) -> i64 {
        self.frequency_ppb
    }

    /// The running average path delay (ns).
    #[must_use]
    pub const fn avg_delay_ns(&self) -> i64 {
        self.avg_delay_ns
    }

    /// The number of samples accepted so far.
    #[must_use]
    pub const fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Whether the servo has anchored on at least one sample.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        self.locked
    }

    /// The estimated master time corresponding to a `local_ns` reading: subtract
    /// the offset (`local - master = offset`, so `master = local - offset`).
    #[must_use]
    pub const fn master_from_local(&self, local_ns: i64) -> i64 {
        local_ns.saturating_sub(self.offset_ns)
    }

    /// Feed one measurement into the servo, returning whether it was **accepted**
    /// (`true`) or rejected as a delay outlier (`false`).
    ///
    /// The first accepted sample anchors the offset exactly. Subsequent samples
    /// are either applied as a hard step (when their offset is further than
    /// [`ServoConfig::step_threshold_ns`] from the current estimate) or
    /// exponentially smoothed toward the new measurement. The frequency estimate
    /// tracks the smoothed change in offset.
    pub fn update(&mut self, sample: PtpSample) -> bool {
        // Outlier guard: reject obviously asymmetric delay spikes once we have a
        // running average to compare against.
        if self.locked && self.config.delay_outlier_pct > 0 && self.avg_delay_ns > 0 {
            // limit = avg_delay * pct / 100, computed in i128 to avoid overflow.
            let limit = i128::from(self.avg_delay_ns)
                .saturating_mul(i128::from(self.config.delay_outlier_pct))
                / 100;
            if i128::from(sample.delay_ns) > limit {
                return false;
            }
        }

        // Update the running average delay (same exponential weight as the
        // offset filter).
        if self.locked {
            let recip = self.config.alpha_recip.max(1);
            let delta = sample.delay_ns.saturating_sub(self.avg_delay_ns);
            self.avg_delay_ns = self.avg_delay_ns.saturating_add(delta.div_euclid(recip));
        } else {
            self.avg_delay_ns = sample.delay_ns;
        }

        if !self.locked {
            // First sample: anchor exactly.
            self.offset_ns = sample.offset_ns;
            self.frequency_ppb = 0;
            self.locked = true;
            self.accepted = self.accepted.saturating_add(1);
            return true;
        }

        let error = sample.offset_ns.saturating_sub(self.offset_ns);
        // `saturating_abs` avoids the `i64::MIN.abs()` overflow panic.
        if error.saturating_abs() > self.config.step_threshold_ns {
            // Large discontinuity: step the estimate (grandmaster change / reset).
            self.offset_ns = sample.offset_ns;
            // A step invalidates the frequency trend; reset it.
            self.frequency_ppb = 0;
        } else {
            // Slew: move 1/alpha of the way toward the new measurement.
            let recip = self.config.alpha_recip.max(1);
            let correction = error.div_euclid(recip);
            self.offset_ns = self.offset_ns.saturating_add(correction);
            // Frequency error tracks the per-sample drift of the smoothed offset,
            // also exponentially smoothed. `correction` is the smoothed offset
            // delta per sample; fold it into the frequency estimate.
            let freq_delta = correction.saturating_sub(self.frequency_ppb);
            self.frequency_ppb = self
                .frequency_ppb
                .saturating_add(freq_delta.div_euclid(recip));
        }
        self.accepted = self.accepted.saturating_add(1);
        true
    }
}

/// Hardware PTP binding — **feature `ptp`, compile-verified only**.
///
/// Reading the host PTP Hardware Clock (PHC) and driving its frequency requires a
/// real PTP-capable NIC and OS clock-discipline syscalls, neither of which exist
/// in this environment. The binding therefore lives behind the off-by-default
/// `ptp` feature and is compile-verified only; its correctness rests on the pure,
/// fully-tested [`PtpServo`] above. Crucially, even with `ptp` enabled the servo
/// disciplines a **separate reference clock** — the output clock's
/// `out_pts = f(tick)` is untouched (see the module docs).
#[cfg(feature = "ptp")]
pub mod phc {
    use super::{PtpSample, PtpServo, ServoConfig};

    /// A disciplined-reference clock driven by the PTP servo from sampled
    /// `(offset, delay)` measurements.
    ///
    /// In a real deployment the samples come from a PTP receiver reading the host
    /// PHC; here the type is constructed and exercised purely through its servo,
    /// so it is meaningful to compile-check but cannot be runtime-verified without
    /// a NIC.
    #[derive(Debug, Clone)]
    pub struct DisciplinedReference {
        servo: PtpServo,
    }

    impl DisciplinedReference {
        /// Build a disciplined reference with the given servo tuning.
        #[must_use]
        pub fn new(config: ServoConfig) -> Self {
            Self {
                servo: PtpServo::new(config),
            }
        }

        /// Apply a PTP measurement (in a real build, read from the PHC); returns
        /// whether the sample was accepted.
        pub fn apply(&mut self, sample: PtpSample) -> bool {
            self.servo.update(sample)
        }

        /// The estimated grandmaster time for a local monotonic reading.
        #[must_use]
        pub const fn master_from_local(&self, local_ns: i64) -> i64 {
            self.servo.master_from_local(local_ns)
        }

        /// Borrow the underlying servo (for metrics).
        #[must_use]
        pub const fn servo(&self) -> &PtpServo {
            &self.servo
        }
    }
}
