#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::float_cmp
)]
//! The drift-alarm hysteresis state machine (DEV-C3, ADR-M010): a member's
//! measured skew exceeding the group's `target_skew_ms` for a dwell **raises**
//! a drift alarm; sustained recovery below the target for a (longer) dwell
//! **clears** it. The two independent dwells give the hysteresis that stops a
//! member hovering at the threshold from flapping the alarm. Pure function of an
//! injected [`MediaTime`] — no real time, no sleeps.

use multiview_control::devices::sync_drift::{DriftHysteresis, DriftMonitor, DriftTransition};
use multiview_core::time::MediaTime;

/// Milliseconds → media time.
fn ms(n: i64) -> MediaTime {
    MediaTime::from_nanos(n.saturating_mul(1_000_000))
}

/// `dwell_up` = 2 s, `dwell_down` = 3 s.
fn hysteresis() -> DriftHysteresis {
    DriftHysteresis::new(ms(2_000), ms(3_000))
}

/// Below target: no alarm, ever.
#[test]
fn within_target_never_raises() {
    let mut mon = DriftMonitor::new(hysteresis());
    let mut now = MediaTime::ZERO;
    for _ in 0..100 {
        now = now.saturating_add(ms(100));
        let t = mon.observe(10.0, 50, now);
        assert_eq!(t, DriftTransition::None);
        assert!(!mon.is_alarmed());
    }
}

/// Exceeding the target for less than `dwell_up` does NOT raise.
#[test]
fn brief_excursion_under_dwell_does_not_raise() {
    let mut mon = DriftMonitor::new(hysteresis());
    // Present at t=0 (starts the dwell), still present at t=1.9 s — under 2 s.
    assert_eq!(
        mon.observe(80.0, 50, MediaTime::ZERO),
        DriftTransition::None
    );
    assert_eq!(mon.observe(80.0, 50, ms(1_900)), DriftTransition::None);
    assert!(!mon.is_alarmed());
}

/// Exceeding the target continuously for `dwell_up` RAISES exactly once.
#[test]
fn sustained_excursion_raises_after_dwell_up() {
    let mut mon = DriftMonitor::new(hysteresis());
    assert_eq!(
        mon.observe(80.0, 50, MediaTime::ZERO),
        DriftTransition::None
    );
    assert_eq!(mon.observe(80.0, 50, ms(1_999)), DriftTransition::None);
    // At exactly dwell_up the alarm raises.
    assert_eq!(mon.observe(80.0, 50, ms(2_000)), DriftTransition::Raised);
    assert!(mon.is_alarmed());
    // It does not re-raise while it stays present.
    assert_eq!(mon.observe(80.0, 50, ms(3_000)), DriftTransition::None);
    assert!(mon.is_alarmed());
}

/// A raised alarm does NOT clear the instant the skew recovers — it must stay
/// recovered for `dwell_down` (hysteresis / anti-flap).
#[test]
fn recovery_under_dwell_down_does_not_clear() {
    let mut mon = DriftMonitor::new(hysteresis());
    mon.observe(80.0, 50, MediaTime::ZERO);
    assert_eq!(mon.observe(80.0, 50, ms(2_000)), DriftTransition::Raised);
    // Recovers at t=2 s; only 2.9 s elapsed by t=4.9 s — under dwell_down (3 s).
    assert_eq!(mon.observe(10.0, 50, ms(2_000)), DriftTransition::None);
    assert_eq!(mon.observe(10.0, 50, ms(4_900)), DriftTransition::None);
    assert!(mon.is_alarmed());
}

/// Sustained recovery for `dwell_down` CLEARS the alarm exactly once.
#[test]
fn sustained_recovery_clears_after_dwell_down() {
    let mut mon = DriftMonitor::new(hysteresis());
    mon.observe(80.0, 50, MediaTime::ZERO);
    assert_eq!(mon.observe(80.0, 50, ms(2_000)), DriftTransition::Raised);
    assert_eq!(mon.observe(10.0, 50, ms(2_000)), DriftTransition::None);
    // 3 s of continuous recovery → clear.
    assert_eq!(mon.observe(10.0, 50, ms(5_000)), DriftTransition::Cleared);
    assert!(!mon.is_alarmed());
}

/// A fault that returns within the clear dwell snaps the alarm back to raised
/// without a clear/re-raise flap.
#[test]
fn fault_returning_within_clear_dwell_does_not_flap() {
    let mut mon = DriftMonitor::new(hysteresis());
    mon.observe(80.0, 50, MediaTime::ZERO);
    assert_eq!(mon.observe(80.0, 50, ms(2_000)), DriftTransition::Raised);
    // Recover briefly, then the skew comes back before dwell_down elapses.
    assert_eq!(mon.observe(10.0, 50, ms(2_500)), DriftTransition::None);
    assert_eq!(mon.observe(80.0, 50, ms(3_000)), DriftTransition::None);
    assert!(mon.is_alarmed());
    // Still alarmed well past where a naive clear would have fired.
    assert_eq!(mon.observe(80.0, 50, ms(9_000)), DriftTransition::None);
    assert!(mon.is_alarmed());
}

/// The threshold is strict: a measurement exactly AT the target is within
/// spec (not exceeded), so it never raises.
#[test]
fn measurement_exactly_at_target_is_within_spec() {
    let mut mon = DriftMonitor::new(hysteresis());
    assert_eq!(
        mon.observe(50.0, 50, MediaTime::ZERO),
        DriftTransition::None
    );
    assert_eq!(mon.observe(50.0, 50, ms(10_000)), DriftTransition::None);
    assert!(!mon.is_alarmed());
}

/// A backwards clock cannot prematurely raise (elapsed clamps to zero).
#[test]
fn backwards_clock_cannot_shorten_the_dwell() {
    let mut mon = DriftMonitor::new(hysteresis());
    assert_eq!(mon.observe(80.0, 50, ms(10_000)), DriftTransition::None);
    // Time goes backwards: elapsed is clamped, so no raise.
    assert_eq!(mon.observe(80.0, 50, ms(0)), DriftTransition::None);
    assert!(!mon.is_alarmed());
}
