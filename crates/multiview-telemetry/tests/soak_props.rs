//! Property tests for the pure acceptance-soak analyzer (DEV-C4, ADR-R012).
//!
//! The verdict logic is the contract a hardware soak is judged by, so its
//! invariants are pinned with `proptest`, not only example tests: the
//! nearest-rank percentile is sign-blind and order-independent and lands inside
//! the sample range, `evaluate_offset`'s pass flag is exactly
//! `p99 <= threshold`, and `cadence_uninterrupted` is true iff every consecutive
//! window advanced at least the floor.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_telemetry::clock::ClockSourceLabel;
use multiview_telemetry::soak::{cadence_uninterrupted, evaluate_offset, p99_abs_offset_ns};
use proptest::prelude::*;

proptest! {
    /// The nearest-rank p99 of a non-empty series is one of the series' own
    /// absolute values, and it lies within [min|x|, max|x|]. It can never invent
    /// a value outside the observed spread.
    #[test]
    fn p99_is_an_observed_absolute_value_within_range(samples in proptest::collection::vec(any::<i64>(), 1..256)) {
        let p99 = p99_abs_offset_ns(&samples).unwrap();
        let abs: Vec<i64> = samples.iter().map(|s| i64::try_from(s.unsigned_abs()).unwrap_or(i64::MAX)).collect();
        let min = *abs.iter().min().unwrap();
        let max = *abs.iter().max().unwrap();
        prop_assert!(p99 >= min, "p99 {p99} below min |x| {min}");
        prop_assert!(p99 <= max, "p99 {p99} above max |x| {max}");
        prop_assert!(abs.contains(&p99), "p99 {p99} is an observed |sample|");
    }

    /// The percentile is independent of input order — shuffling the samples must
    /// not change the verdict (it sorts internally).
    #[test]
    fn p99_is_order_independent(mut samples in proptest::collection::vec(any::<i64>(), 1..128), seed in any::<u64>()) {
        let original = p99_abs_offset_ns(&samples);
        // A cheap deterministic permutation driven by `seed`.
        let n = samples.len();
        let mut s = seed;
        for i in (1..n).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (s >> 33) as usize % (i + 1);
            samples.swap(i, j);
        }
        prop_assert_eq!(original, p99_abs_offset_ns(&samples));
    }

    /// The percentile sees absolute magnitude: negating every sample leaves the
    /// p99 of |offset| unchanged (a sign flip cannot hide drift).
    #[test]
    fn p99_is_sign_blind(samples in proptest::collection::vec(-1_000_000_000i64..=1_000_000_000, 1..128)) {
        let pos = p99_abs_offset_ns(&samples);
        let neg: Vec<i64> = samples.iter().map(|s| -s).collect();
        prop_assert_eq!(pos, p99_abs_offset_ns(&neg));
    }

    /// `evaluate_offset`'s pass flag is exactly `p99_abs_ns <= threshold_ns` for
    /// the leg's own bound — no off-by-one at the boundary, either source.
    #[test]
    fn evaluate_offset_pass_is_p99_le_threshold(
        samples in proptest::collection::vec(0i64..=5_000_000, 1..128),
        ptp in any::<bool>(),
    ) {
        let source = if ptp { ClockSourceLabel::Ptp } else { ClockSourceLabel::System };
        let v = evaluate_offset(source, &samples).unwrap();
        prop_assert_eq!(v.threshold_ns, source.offset_p99_max_ns());
        prop_assert_eq!(v.pass, v.p99_abs_ns <= v.threshold_ns);
        prop_assert_eq!(v.samples, samples.len());
    }

    /// `cadence_uninterrupted` is true iff every consecutive window advanced at
    /// least `floor` — equivalently, false iff any window fell short.
    #[test]
    fn cadence_is_true_iff_no_window_falls_short(
        ticks in proptest::collection::vec(any::<u64>(), 0..64),
        floor in 0u64..=120,
    ) {
        let expected = ticks
            .windows(2)
            .all(|w| w[1].saturating_sub(w[0]) >= floor);
        prop_assert_eq!(cadence_uninterrupted(&ticks, floor), expected);
    }

    /// A strictly-monotone series advancing by at least the floor each step is
    /// always continuous; a single deliberately-flat window (a stall) always
    /// fails for any positive floor.
    #[test]
    fn a_single_stall_always_fails(
        len in 2usize..32,
        floor in 1u64..=60,
        stall_at in 0usize..30,
    ) {
        // Build a series that advances by exactly `floor` each step.
        let mut ticks: Vec<u64> = (0..len as u64).map(|i| i * floor).collect();
        prop_assert!(cadence_uninterrupted(&ticks, floor), "monotone-by-floor is continuous");
        // Now flatten one interior window: repeat the previous value.
        let idx = 1 + (stall_at % (len - 1));
        ticks[idx] = ticks[idx - 1];
        prop_assert!(!cadence_uninterrupted(&ticks, floor), "a flat window must fail");
    }
}
