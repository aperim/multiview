//! The drift-alarm hysteresis state machine for a sync-group member (DEV-C3,
//! ADR-M010).
//!
//! A sync group declares a `target_skew_ms` — the largest presentation skew a
//! member may show before it is *degraded*. A member whose measured skew exceeds
//! the target **continuously for a dwell window** raises a `degraded-sync` drift
//! alarm; a member whose skew recovers below the target **continuously for a
//! (longer) dwell window** clears it. The two independent dwells give the
//! hysteresis that stops a member hovering at the threshold from flapping the
//! alarm.
//!
//! This is a pure value-machine over an injected [`MediaTime`] — the same
//! classify-style total transition the engine's X.733
//! `multiview_engine::alarm::state::AlarmStateMachine` uses, focused to the one
//! present/absent drift condition and living in the control plane (sync groups
//! are control-plane only, invariant #10). It is exhaustively testable with a
//! synthetic clock — no real time, no sleeps.

use multiview_core::time::MediaTime;

/// How long the skew must stay above (raise) / below (clear) the target before
/// the drift alarm changes state.
///
/// `dwell_up` is how long the skew must be **continuously over** the target
/// before the alarm raises; `dwell_down` is how long it must be **continuously
/// back inside** the target before a raised alarm clears. A longer `dwell_down`
/// is the anti-flap bias (we are quicker to warn than to declare recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriftHysteresis {
    dwell_up: MediaTime,
    dwell_down: MediaTime,
}

impl Default for DriftHysteresis {
    /// The default drift dwell windows: raise after **3 s** continuously over
    /// target, clear after **5 s** continuously recovered.
    ///
    /// Drift is a slow signal sampled at the ~1 Hz device-poll cadence, so a
    /// few-second dwell rejects a single noisy sample without masking a real
    /// sustained degradation; the longer clear bias avoids declaring recovery
    /// prematurely. These are sensible defaults (the `SyncGroup` config carries
    /// only `target_skew_ms`, not a per-group dwell); a future ADR may surface
    /// per-group dwell knobs.
    fn default() -> Self {
        Self {
            dwell_up: MediaTime::from_nanos(3_000_000_000),
            dwell_down: MediaTime::from_nanos(5_000_000_000),
        }
    }
}

impl DriftHysteresis {
    /// Construct from explicit dwell windows. Negative inputs clamp to
    /// [`MediaTime::ZERO`] so a window is never "in the past".
    #[must_use]
    pub fn new(dwell_up: MediaTime, dwell_down: MediaTime) -> Self {
        Self {
            dwell_up: non_negative(dwell_up),
            dwell_down: non_negative(dwell_down),
        }
    }

    /// The raise (dwell-up) window.
    #[must_use]
    pub const fn dwell_up(self) -> MediaTime {
        self.dwell_up
    }

    /// The clear (dwell-down) window.
    #[must_use]
    pub const fn dwell_down(self) -> MediaTime {
        self.dwell_down
    }
}

/// The lifecycle phase of a member's drift condition.
///
/// `Pending`/`Clearing` are the transitional phases where a dwell is being
/// served; `Clear`/`Raised` are stable. `since` records when the current
/// transitional phase began so the dwell can be measured against `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Within target; no drift alarm.
    Clear,
    /// Over target but `dwell_up` has not yet elapsed; began at `since`.
    Pending { since: MediaTime },
    /// The drift alarm is active (over target for `dwell_up`).
    Raised,
    /// Active but recovered; `dwell_down` has not yet elapsed; began at `since`.
    Clearing { since: MediaTime },
}

/// What changed as a result of a [`DriftMonitor::observe`] step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DriftTransition {
    /// No active-state change this step (still within, or still alarmed).
    None,
    /// The drift alarm became active this step (skew over target for the dwell).
    Raised,
    /// The drift alarm cleared this step (skew recovered for the clear dwell).
    Cleared,
}

/// A per-member drift alarm with dwell-up / dwell-down hysteresis.
///
/// Construct with [`DriftMonitor::new`], then drive it once per status sample
/// with [`DriftMonitor::observe`] (the member's measured skew, the group's
/// target, and the current [`MediaTime`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftMonitor {
    hysteresis: DriftHysteresis,
    phase: Phase,
}

impl DriftMonitor {
    /// A fresh, clear drift monitor with the given hysteresis.
    #[must_use]
    pub fn new(hysteresis: DriftHysteresis) -> Self {
        Self {
            hysteresis,
            phase: Phase::Clear,
        }
    }

    /// Whether the drift alarm is currently active (`Raised` or `Clearing`).
    #[must_use]
    pub const fn is_alarmed(&self) -> bool {
        matches!(self.phase, Phase::Raised | Phase::Clearing { .. })
    }

    /// Drive the machine one step with the member's `measured_skew_ms` against
    /// the group's `target_skew_ms` at media time `now`, applying
    /// dwell-up/dwell-down hysteresis.
    ///
    /// The drift condition is **present** when the measured skew strictly
    /// exceeds the target (a measurement exactly at the target is within spec).
    /// `now` running backwards cannot shorten a dwell: elapsed time clamps to
    /// zero, so a backwards step never prematurely raises or clears.
    pub fn observe(
        &mut self,
        measured_skew_ms: f32,
        target_skew_ms: u32,
        now: MediaTime,
    ) -> DriftTransition {
        // Strictly-greater: at exactly the target the member is within spec.
        // NaN (no real measurement) is treated as absent, never alarming.
        let present = measured_skew_ms > skew_target_ms(target_skew_ms);

        // Fold the new sample into the phase (a transitional phase carries the
        // instant the current run of present/absent samples began).
        match self.phase {
            Phase::Clear => {
                if present {
                    self.phase = Phase::Pending { since: now };
                }
            }
            Phase::Pending { .. } => {
                if !present {
                    self.phase = Phase::Clear;
                }
            }
            Phase::Raised => {
                if !present {
                    self.phase = Phase::Clearing { since: now };
                }
            }
            Phase::Clearing { .. } => {
                if present {
                    // Drift returned within the clear dwell: snap back to active.
                    self.phase = Phase::Raised;
                }
            }
        }

        // Serve any due dwell on the resulting transitional phase.
        match self.phase {
            Phase::Pending { since } if elapsed(since, now) >= self.hysteresis.dwell_up => {
                self.phase = Phase::Raised;
                DriftTransition::Raised
            }
            Phase::Clearing { since } if elapsed(since, now) >= self.hysteresis.dwell_down => {
                self.phase = Phase::Clear;
                DriftTransition::Cleared
            }
            _ => DriftTransition::None,
        }
    }
}

/// The skew target as `f32` milliseconds for comparison.
fn skew_target_ms(target_skew_ms: u32) -> f32 {
    // `u32` skew targets are capped at 10_000 ms by config validation, so this
    // widening is exact in `f32`.
    f32::from(u16::try_from(target_skew_ms).unwrap_or(u16::MAX))
}

/// Elapsed time from `since` to `now`, clamped non-negative so a clock that
/// momentarily runs backwards cannot shorten a dwell.
fn elapsed(since: MediaTime, now: MediaTime) -> MediaTime {
    non_negative(now.saturating_sub(since))
}

/// Clamp a [`MediaTime`] to non-negative.
fn non_negative(t: MediaTime) -> MediaTime {
    if t.as_nanos() < 0 {
        MediaTime::ZERO
    } else {
        t
    }
}
