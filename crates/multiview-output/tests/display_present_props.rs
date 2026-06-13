//! Property tests for the DEV-C2 pull-side presentation discipline (display-out
//! §8): the pure `choose_frame` decision, the exact-rational flip-anchored
//! `VblankPredictor`, and the bounded drop-oldest `PresentQueue`. The
//! guardrails mandate property tests for pure logic; these pin the invariants
//! the example-based `display_present` tests assert by example.
//!
//! All arithmetic is exact integer ns (invariant #3 — never float fps): the
//! generators stay well inside `i64` so no saturation masks a real defect.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{MediaTime, Rational};
use multiview_core::wallclock::WallClockRef;
use multiview_output::display::{
    choose_frame, FrameChoice, PresentQueue, VblankPredictor, PRESENT_QUEUE_DEPTH,
};
use proptest::prelude::*;

/// Deadlines and a vblank that stay within ±2^40 ns (~18 min) — far inside the
/// `i64` headroom, so `2·(d−V)` never saturates and the property reasons about
/// the exact arithmetic, not the clamp.
fn deadlines_and_vblank() -> impl Strategy<Value = (Vec<i64>, i64, i64)> {
    (
        proptest::collection::vec(
            -1_099_511_627_776i64..1_099_511_627_776,
            0..=PRESENT_QUEUE_DEPTH,
        ),
        -1_099_511_627_776i64..1_099_511_627_776,
        1i64..200_000_000, // a positive period (≤ 0.2 s)
    )
        .prop_map(|(deadlines, vblank, period)| (deadlines, vblank, period))
}

proptest! {
    /// A `Present { index }` is always in range; `Idle` only on an empty queue.
    #[test]
    fn the_choice_index_is_always_in_range((deadlines, vblank, period) in deadlines_and_vblank()) {
        match choose_frame(&deadlines, vblank, period) {
            FrameChoice::Idle => prop_assert!(deadlines.is_empty()),
            FrameChoice::RepeatEarly => prop_assert!(!deadlines.is_empty()),
            FrameChoice::Present { index } => prop_assert!(index < deadlines.len()),
            other => prop_assert!(false, "unexpected non-exhaustive choice: {other:?}"),
        }
    }

    /// When a frame is presented (not repeat-early, non-degenerate, non-empty),
    /// the chosen frame is a true argmin of `|deadline − vblank|`: no other
    /// queued frame is strictly nearer the predicted vblank.
    #[test]
    fn a_present_choice_is_the_nearest_deadline(
        (deadlines, vblank, period) in deadlines_and_vblank()
    ) {
        if let FrameChoice::Present { index } = choose_frame(&deadlines, vblank, period) {
            let chosen = (i128::from(deadlines[index]) - i128::from(vblank)).abs();
            for &d in &deadlines {
                let other = (i128::from(d) - i128::from(vblank)).abs();
                prop_assert!(chosen <= other, "chosen {chosen} not nearest (saw {other})");
            }
        }
    }

    /// Repeat-early is EXACTLY the case where the nearest frame is more than half
    /// a period in the future (`2·(d−V) > period`): the decision and the
    /// arithmetic predicate agree, both directions.
    #[test]
    fn repeat_early_iff_the_nearest_frame_is_more_than_half_a_period_ahead(
        (deadlines, vblank, period) in deadlines_and_vblank()
    ) {
        prop_assume!(!deadlines.is_empty());
        let choice = choose_frame(&deadlines, vblank, period);
        // Recompute the nearest (ties → higher index, matching the impl).
        let mut best = deadlines.len() - 1;
        let mut best_dist = i128::MAX;
        for (i, &d) in deadlines.iter().enumerate() {
            let dist = (i128::from(d) - i128::from(vblank)).abs();
            if dist <= best_dist {
                best_dist = dist;
                best = i;
            }
        }
        let ahead = i128::from(deadlines[best]) - i128::from(vblank);
        let is_early = ahead * 2 > i128::from(period);
        prop_assert_eq!(choice == FrameChoice::RepeatEarly, is_early);
    }

    /// A degenerate (≤ 0) period never disciplines: it always presents the
    /// newest (last) frame when the queue is non-empty, else idles.
    #[test]
    fn a_degenerate_period_always_presents_newest(
        deadlines in proptest::collection::vec(any::<i64>(), 0..=PRESENT_QUEUE_DEPTH),
        vblank in any::<i64>(),
        period in i64::MIN..=0,
    ) {
        match choose_frame(&deadlines, vblank, period) {
            FrameChoice::Idle => prop_assert!(deadlines.is_empty()),
            FrameChoice::Present { index } => prop_assert_eq!(index, deadlines.len() - 1),
            FrameChoice::RepeatEarly => {
                prop_assert!(false, "degenerate period must never repeat-early");
            }
            other => prop_assert!(false, "unexpected non-exhaustive choice: {other:?}"),
        }
    }

    /// The predicted vblank is always strictly after `now`, lands exactly on the
    /// `anchor + k·period` grid, and is within one period of `now`.
    #[test]
    fn the_prediction_is_the_next_grid_instant(
        // 60000/1001 is the tightest realistic non-integer period; keep nums
        // positive so the period is well-defined.
        num in 1i64..=240_000,
        den in 1i64..=1_001,
        anchor in -1_000_000_000_000i64..1_000_000_000_000,
        delta in 0i64..1_000_000_000_000, // now = anchor + delta (always ≥ anchor)
    ) {
        let refresh = Rational::new(num, den);
        let period = MediaTime::from_tick(1, refresh).as_nanos();
        prop_assume!(period > 0);
        let mut predictor = VblankPredictor::new(refresh);
        predictor.on_flip(anchor);
        let now = anchor.saturating_add(delta);
        let predicted = predictor.predicted_next_ns(now).expect("anchored, positive period");
        prop_assert!(predicted > now, "vblank {predicted} not strictly after now {now}");
        prop_assert!(
            (i128::from(predicted) - i128::from(anchor)) % i128::from(period) == 0,
            "vblank {predicted} is off the anchor+{period}·k grid"
        );
        prop_assert!(
            i128::from(predicted) - i128::from(now) <= i128::from(period),
            "vblank {predicted} is more than one period past now {now}"
        );
    }

    /// Before any flip, the predictor never predicts (no phase). A degenerate
    /// refresh never predicts even after a flip.
    #[test]
    fn no_anchor_or_degenerate_refresh_never_predicts(
        now in any::<i64>(),
        flip in any::<i64>(),
    ) {
        prop_assert_eq!(VblankPredictor::new(Rational::new(50, 1)).predicted_next_ns(now), None);
        let mut degenerate = VblankPredictor::new(Rational::new(0, 1));
        degenerate.on_flip(flip);
        prop_assert_eq!(degenerate.predicted_next_ns(now), None);
    }

    /// The queue is bounded: it never exceeds the depth, `push` reports overflow
    /// exactly when it was already full, and after every push the newest frame
    /// is at the back (newest-wins — drop-oldest).
    #[test]
    fn the_queue_is_bounded_and_newest_wins(
        seqs in proptest::collection::vec(1u64..1_000_000, 0..32),
    ) {
        let mut queue: PresentQueue<u64> = PresentQueue::new();
        for (i, &seq) in seqs.iter().enumerate() {
            let was_full = queue.len() == PRESENT_QUEUE_DEPTH;
            let overflow = queue.push(seq, seq, i64::try_from(seq).unwrap_or(0));
            prop_assert_eq!(overflow, was_full, "overflow flag must equal was-full");
            prop_assert!(queue.len() <= PRESENT_QUEUE_DEPTH, "queue grew past the bound");
            // The just-pushed frame is the newest (back of the queue).
            let last = queue.len() - 1;
            let (frame, _s, _p) = queue.entry(last).expect("the back entry exists");
            prop_assert_eq!(*frame, seq);
            let _ = i;
        }
    }

    /// `deadlines(epoch, link)` is exactly `wall_at(pts) + link` per entry, in
    /// order, with one value per queued frame.
    #[test]
    fn deadlines_are_wall_at_plus_link_offset(
        ptses in proptest::collection::vec(-1_000_000_000i64..1_000_000_000, 0..=PRESENT_QUEUE_DEPTH),
        wall_anchor in -1_000_000_000_000i64..1_000_000_000_000,
        link in 0i64..1_000_000_000,
    ) {
        let mut queue: PresentQueue<()> = PresentQueue::new();
        for (i, &pts) in ptses.iter().enumerate() {
            queue.push((), u64::try_from(i).unwrap_or(0) + 1, pts);
        }
        // 1 GHz epoch (the canonical ns-domain outbound presentation epoch).
        let epoch = WallClockRef::new(wall_anchor, 0, Rational::new(1_000_000_000, 1));
        let deadlines = queue.deadlines(epoch, link);
        prop_assert_eq!(deadlines.len(), ptses.len());
        for (got, &pts) in deadlines.iter().zip(ptses.iter()) {
            prop_assert_eq!(*got, epoch.wall_at(pts).saturating_add(link));
        }
    }
}
