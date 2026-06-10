//! DEV-C1 (ADR-M010): the **outbound presentation epoch** — one
//! `WallClockRef` per program anchoring the output tick counter to disciplined
//! wall-clock nanoseconds.
//!
//! Pins:
//! * the `EpochTracker` re-anchor policy: anchor once, **hold** inside the
//!   deadband, **slew** (bounded) toward a persistent error, **step** only on a
//!   gross discontinuity (the documented Class-2-like case);
//! * the `EpochSampler` anchor math is exact integer (`wall@tick0 = wall_now −
//!   (mono_mid − seed)`), with the PTP servo offset applied only on the PTP leg;
//! * ADR-T012 source precedence: PTP-if-disciplined, else the system clock,
//!   carried honestly as `ClockSource`/`ClockQuality`;
//! * invariant #1: epoch observation can never change the tick stream.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_core::time::{MediaTime, Rational};
use multiview_core::wallclock::WallClockRef;
use multiview_engine::epoch::{
    EpochPolicy, EpochSampler, EpochSamplerConfig, EpochTracker, EpochUpdate, WallClockSampler,
    WallSample, EPOCH_RATE,
};
use multiview_engine::ptp::{LockState, ReferenceStatus};
use multiview_engine::sysref::{NtpQuery, NtpReading, SystemRefConfig};
use multiview_engine::{LatestState, OutputClock};
use multiview_events::{ClockQuality, ClockSource};
use proptest::prelude::*;

/// A policy with easy-to-reason-about thresholds for the tests.
fn policy() -> EpochPolicy {
    EpochPolicy {
        deadband_ns: 2_000_000,         // 2 ms
        slew_max_ns: 5_000_000,         // 5 ms per observation
        step_threshold_ns: 250_000_000, // 250 ms
    }
}

/// An epoch candidate anchored at media 0 with the canonical ns rate.
fn candidate(wall_at_anchor_ns: i64) -> WallClockRef {
    WallClockRef::new(wall_at_anchor_ns, 0, EPOCH_RATE)
}

// ---------------------------------------------------------------------------
// EpochTracker: hold / slew / step
// ---------------------------------------------------------------------------

#[test]
fn tracker_anchors_on_the_first_observation() {
    let mut t = EpochTracker::new(policy());
    assert_eq!(t.current(), None, "no epoch before the first observation");
    let c = candidate(1_750_000_000_000_000_000);
    assert_eq!(t.observe(c), EpochUpdate::Anchored);
    assert_eq!(
        t.current(),
        Some(c),
        "the first observation anchors exactly"
    );
}

#[test]
fn tracker_holds_inside_the_deadband() {
    let mut t = EpochTracker::new(policy());
    let c = candidate(1_750_000_000_000_000_000);
    let _ = t.observe(c);
    // 1.5 ms of wall jitter is inside the 2 ms deadband: the published epoch
    // must NOT move (consumers present against a stable map).
    let jittered = candidate(1_750_000_000_001_500_000);
    assert_eq!(t.observe(jittered), EpochUpdate::Held);
    assert_eq!(
        t.current(),
        Some(c),
        "the epoch must hold inside the deadband"
    );
}

#[test]
fn tracker_slews_bounded_toward_a_persistent_error() {
    let mut t = EpochTracker::new(policy());
    let _ = t.observe(candidate(1_750_000_000_000_000_000));
    // A persistent +20 ms error (above deadband, below step threshold) must be
    // chased smoothly: each observation moves the anchor by AT MOST slew_max.
    let target = candidate(1_750_000_000_020_000_000);
    assert_eq!(
        t.observe(target),
        EpochUpdate::Slewed {
            adjustment_ns: 5_000_000
        }
    );
    let after_one = t.current().expect("anchored");
    assert_eq!(
        after_one.wall_at_anchor_ns, 1_750_000_000_005_000_000,
        "exactly one slew_max step toward the candidate"
    );
    // Three more observations converge it (5+5+5+5 = 20 ms), then it holds.
    let _ = t.observe(target);
    let _ = t.observe(target);
    let _ = t.observe(target);
    assert_eq!(t.current(), Some(target), "the slew converges exactly");
    assert_eq!(t.observe(target), EpochUpdate::Held);
}

#[test]
fn tracker_steps_on_a_gross_discontinuity() {
    let mut t = EpochTracker::new(policy());
    let _ = t.observe(candidate(1_750_000_000_000_000_000));
    // A 2 s discontinuity (host clock stepped / source changed): re-anchor in
    // one step — the documented Class-2-like re-anchor.
    let stepped = candidate(1_750_000_002_000_000_000);
    assert_eq!(
        t.observe(stepped),
        EpochUpdate::Stepped {
            error_ns: 2_000_000_000
        }
    );
    assert_eq!(t.current(), Some(stepped));
}

#[test]
fn tracker_re_anchors_when_the_rate_changes() {
    let mut t = EpochTracker::new(policy());
    let _ = t.observe(candidate(1_750_000_000_000_000_000));
    // A different media rate is a different timeline: full re-anchor (step),
    // never a cross-rate slew.
    let other_rate = WallClockRef::new(1_750_000_000_000_000_000, 0, Rational::new(90_000, 1));
    assert!(matches!(t.observe(other_rate), EpochUpdate::Stepped { .. }));
    assert_eq!(t.current(), Some(other_rate));
}

#[test]
fn tracker_never_moves_the_media_anchor_on_hold_or_slew() {
    let mut t = EpochTracker::new(policy());
    let first = WallClockRef::new(1_750_000_000_000_000_000, 123_456, EPOCH_RATE);
    let _ = t.observe(first);
    // Candidates carry a different media anchor for the same affine map; the
    // tracker normalises the error at ITS anchor and adjusts only the wall leg.
    let moved = WallClockRef::new(1_750_000_000_020_123_456, 20_000_000 + 123_456, EPOCH_RATE);
    let _ = t.observe(moved);
    let cur = t.current().expect("anchored");
    assert_eq!(cur.media_at_anchor, 123_456, "media anchor is stable");
    assert_eq!(cur.rate, EPOCH_RATE, "rate is stable");
}

proptest! {
    /// Per observation the published wall mapping moves by at most `slew_max`
    /// unless the error exceeds the step threshold (then it lands exactly on
    /// the candidate). Never a panic, never an unbounded move.
    #[test]
    fn prop_tracker_moves_are_bounded(
        anchor in -1_000_000_000_000_000i64..1_000_000_000_000_000i64,
        errors in proptest::collection::vec(-400_000_000i64..400_000_000i64, 1..20),
    ) {
        let p = policy();
        let mut t = EpochTracker::new(p);
        let _ = t.observe(candidate(anchor));
        let mut last = t.current().unwrap().wall_at_anchor_ns;
        for e in errors {
            let target = candidate(last.saturating_add(e));
            let _ = t.observe(target);
            let now = t.current().unwrap().wall_at_anchor_ns;
            let moved = now - last;
            if e.abs() > p.step_threshold_ns {
                prop_assert_eq!(now, target.wall_at_anchor_ns, "gross error steps exactly");
            } else {
                prop_assert!(moved.abs() <= p.slew_max_ns, "move {} exceeds slew bound", moved);
            }
            last = now;
        }
    }
}

// ---------------------------------------------------------------------------
// EpochSampler: anchor math + ADR-T012 source/quality selection
// ---------------------------------------------------------------------------

/// A deterministic wall sampler the tests control.
struct FakeWall {
    mono_mid_ns: i64,
    wall_ns: i64,
}

impl WallClockSampler for FakeWall {
    fn sample(&mut self) -> WallSample {
        WallSample {
            mono_before_ns: self.mono_mid_ns - 1_000,
            wall_ns: self.wall_ns,
            mono_after_ns: self.mono_mid_ns + 1_000,
        }
    }
}

/// An `NtpQuery` the tests control: `Some(reading)` or unavailable.
struct FakeNtp(Option<NtpReading>);

impl NtpQuery for FakeNtp {
    fn read(&mut self) -> Option<NtpReading> {
        self.0
    }
}

fn sampler_config() -> EpochSamplerConfig {
    EpochSamplerConfig {
        policy: policy(),
        sys: SystemRefConfig::new_default(),
    }
}

fn ptp_status(state: LockState, offset_ns: i64) -> ReferenceStatus {
    ReferenceStatus {
        state,
        offset_ns,
        frequency_ppb: 0,
        accepted: 16,
        disciplined: state.is_disciplined(),
    }
}

#[test]
fn sampler_derives_the_anchor_exactly_from_the_system_clock() {
    // seed = 2 s on the monotonic timeline; the wall sample is taken at
    // mono 5 s, wall W. Tick 0's wall instant is exactly W - 3 s.
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let mut s = EpochSampler::new(2_000_000_000, wall, FakeNtp(None), sampler_config());
    let status = s.sample_once();
    assert_eq!(status.epoch.rate, EPOCH_RATE);
    assert_eq!(
        status.epoch.media_at_anchor, 0,
        "anchored at tick 0 (pts 0)"
    );
    assert_eq!(
        status.epoch.wall_at(0),
        1_700_000_000_000_000_000 - 3_000_000_000,
        "wall@tick0 = wall_now - (mono_mid - seed), exact integer"
    );
    // No PTP handle + an unavailable kernel read: the assumed-NTP-disciplined
    // system clock is the honest source (the documented deployment assumption).
    assert_eq!(status.source, ClockSource::System);
    assert_eq!(status.quality, ClockQuality::Locked);
}

#[test]
fn sampler_prefers_a_disciplined_ptp_reference_and_applies_its_offset() {
    let wall = FakeWall {
        mono_mid_ns: 5_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    // local - master = +250 ns: the local clock is ahead, so the disciplined
    // wall estimate is local - offset.
    handle.publish(ptp_status(LockState::Locked, 250));
    let mut s =
        EpochSampler::new(2_000_000_000, wall, FakeNtp(None), sampler_config()).with_ptp(handle);
    let status = s.sample_once();
    assert_eq!(status.source, ClockSource::Ptp);
    assert_eq!(status.quality, ClockQuality::Locked);
    assert_eq!(
        status.epoch.wall_at(0),
        (1_700_000_000_000_000_000 - 250) - 3_000_000_000,
        "the PTP leg disciplines the wall estimate: wall - servo offset"
    );
}

#[test]
fn sampler_keeps_ptp_through_holdover_but_not_acquiring() {
    let mk = |state: LockState| {
        let wall = FakeWall {
            mono_mid_ns: 5_000_000_000,
            wall_ns: 1_700_000_000_000_000_000,
        };
        let handle: LatestState<ReferenceStatus> = LatestState::new();
        handle.publish(ptp_status(state, 0));
        let mut s = EpochSampler::new(0, wall, FakeNtp(None), sampler_config()).with_ptp(handle);
        s.sample_once()
    };
    // Holdover is still disciplined: PTP stays selected (ADR-T012 §1).
    let h = mk(LockState::Holdover);
    assert_eq!(h.source, ClockSource::Ptp);
    assert_eq!(h.quality, ClockQuality::Holdover);
    // Acquiring is NOT disciplined: selection falls to the system clock.
    let a = mk(LockState::Acquiring);
    assert_eq!(a.source, ClockSource::System);
}

#[test]
fn sampler_reports_freerun_honestly() {
    // An operator who knows the host is undisciplined configures the assumed
    // state Freerun; with no PTP the published quality must say so.
    let wall = FakeWall {
        mono_mid_ns: 1_000_000_000,
        wall_ns: 1_700_000_000_000_000_000,
    };
    let config = EpochSamplerConfig {
        policy: policy(),
        sys: SystemRefConfig {
            est_error_tolerance_ns: 100_000,
            assumed_when_unavailable: LockState::Freerun,
        },
    };
    let mut s = EpochSampler::new(0, wall, FakeNtp(None), config);
    let status = s.sample_once();
    assert_eq!(status.source, ClockSource::System);
    assert_eq!(status.quality, ClockQuality::Freerun);
    // It still publishes a usable epoch — quality labels honesty, it never
    // withholds the map (a consumer free-runs drift-bounded).
    assert_eq!(status.epoch.media_at_anchor, 0);
}

#[test]
fn sampler_holds_the_epoch_across_wall_jitter() {
    // Two samples 1 ms apart in wall terms (inside the deadband): the SECOND
    // sample must not move the published epoch.
    struct JitterWall {
        calls: u32,
    }
    impl WallClockSampler for JitterWall {
        fn sample(&mut self) -> WallSample {
            self.calls += 1;
            let wall = if self.calls == 1 {
                1_700_000_000_000_000_000
            } else {
                1_700_000_000_000_000_000 + 1_000_000 // +1 ms jitter
            };
            WallSample {
                mono_before_ns: 5_000_000_000,
                wall_ns: wall,
                mono_after_ns: 5_000_000_000,
            }
        }
    }
    let mut s = EpochSampler::new(0, JitterWall { calls: 0 }, FakeNtp(None), sampler_config());
    let first = s.sample_once();
    let second = s.sample_once();
    assert_eq!(
        first.epoch, second.epoch,
        "wall jitter inside the deadband must not move the epoch"
    );
    assert_eq!(second.update, EpochUpdate::Held);
}

// ---------------------------------------------------------------------------
// Invariant #1: epoch observation never paces the tick stream
// ---------------------------------------------------------------------------

/// A pathological wall source: jumps hours back and forth every sample.
struct WildWall {
    i: i64,
}

impl WallClockSampler for WildWall {
    fn sample(&mut self) -> WallSample {
        self.i += 1;
        let jump = if self.i % 2 == 0 {
            3_600_000_000_000i64
        } else {
            -7_200_000_000_000i64
        };
        WallSample {
            mono_before_ns: self.i.saturating_mul(7),
            wall_ns: 1_700_000_000_000_000_000 + jump,
            mono_after_ns: self.i.saturating_mul(7) + 2,
        }
    }
}

/// Run an output clock for `n` ticks, optionally sampling a chaotic epoch
/// observer between ticks, and collect every PTS.
fn run_clock(n: u64, observe_epoch: bool) -> Vec<MediaTime> {
    let mut clock = OutputClock::new(Rational::FPS_59_94).expect("valid cadence");
    let handle: LatestState<ReferenceStatus> = LatestState::new();
    let mut sampler = EpochSampler::new(0, WildWall { i: 0 }, FakeNtp(None), sampler_config())
        .with_ptp(handle.clone());
    let mut out = Vec::new();
    for i in 0..n {
        if observe_epoch {
            // Flap the PTP reference wildly and re-sample the epoch every tick:
            // none of this may change how many frames are emitted, nor when.
            let state = match i % 4 {
                0 => LockState::Locked,
                1 => LockState::Holdover,
                2 => LockState::Freerun,
                _ => LockState::Acquiring,
            };
            let off = i64::try_from(i).unwrap_or(0).saturating_mul(1_000_003);
            handle.publish(ptp_status(state, off));
            let _ = sampler.sample_once();
        }
        out.push(clock.tick().pts);
    }
    out
}

#[test]
fn epoch_observation_does_not_change_the_tick_stream() {
    const TICKS: u64 = 50_000;
    let baseline = run_clock(TICKS, false);
    let observed = run_clock(TICKS, true);
    assert_eq!(
        baseline, observed,
        "out_pts = f(tick) must be identical with the epoch sampler churning vs off (inv #1)"
    );
}
