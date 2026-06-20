//! The transport command mailbox: a bounded, **conflated latest-wins** seam
//! from the control plane (the CLI frame-boundary command drain) to a media
//! player's ingest thread.
//!
//! # Why conflated-latest, not a queue
//!
//! Transport verbs are operator-rate, idempotent state transitions; the only
//! thing the ingest thread needs is the *most recent* intent per logical slot,
//! sampled between frames. A growable queue would violate the bounded-memory
//! data-plane rule (safety §5) and could let a burst of verbs lag playout. So
//! the mailbox holds **one slot of pending verbs**, drained wholesale by the
//! thread between frames; the writer never blocks and the reader never awaits
//! (invariants #1/#10). Play/pause/stop/vamp/exit collapse to the latest;
//! load/cue/seek carry a target so they are applied in submission order within
//! a single drain.
//!
//! The mailbox is the *only* shared mutable seam; the [`super::MediaPlayer`]
//! itself lives wholly on the ingest thread (no lock on the hot path).

use std::sync::Mutex;

/// A transport verb delivered to a player thread (the control-plane intent;
/// the thread maps it onto [`super::MediaPlayer`] verbs + the executor's
/// seek/decode work). Frame targets are **integer frames** at the asset
/// cadence (inv #3).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TransportVerb {
    /// Bind an asset and (re)open at its in-point.
    Load {
        /// The asset id to bind.
        asset: String,
    },
    /// Park on a frame (or the in-point) and publish it (cue = pre-warm).
    Cue {
        /// The target frame, or `None` for the in-point.
        frame: Option<u64>,
    },
    /// Begin/resume forward playback.
    Play,
    /// Begin vamping the vamp segment.
    Vamp,
    /// Pause on the current frame.
    Pause,
    /// Stop and re-cue to the in-point.
    Stop,
    /// Seek to an exact frame.
    Seek {
        /// The target frame.
        frame: u64,
    },
    /// Arm the vamp exit (fires at the next vamp boundary).
    ArmExit,
    /// Take the vamp exit (arm for the soonest boundary).
    TakeExit,
    /// Cancel a pending vamp exit.
    CancelExit,
}

/// The maximum number of **targeted** verbs (load/cue/seek) held pending before
/// the oldest is dropped. Small: a player polls its mailbox every frame, so more
/// than a handful of un-drained distinct seeks means a stalled consumer, and the
/// newest intents are the ones worth keeping. State-collapsing verbs do not
/// count against this (they conflate to at most one).
const MAX_TARGETED_PENDING: usize = 16;

/// The shared transport mailbox. Two classes of verb, two bounded disciplines —
/// **the queue can never grow without bound** (inv #10, safety §5):
///
/// - **State-collapsing** verbs (play/vamp/pause/stop/arm/take/cancel-exit)
///   are **conflated latest-wins**: a new one supersedes any earlier pending
///   state verb, so at most one is ever held.
/// - **Targeted** verbs (load/cue/seek) carry distinct targets, so they are
///   held in a **bounded FIFO** (cap [`MAX_TARGETED_PENDING`], **drop-oldest**
///   on overflow with a warning) — order-preserving (each distinct seek is
///   honoured in submission order; they are **not** collapsed latest-wins,
///   which would lose intended intermediate seeks).
///
/// Drained wholesale between frames. Cloneable handle semantics: both the
/// control-plane writer and the ingest thread hold an [`std::sync::Arc`] to the
/// same `TransportMailbox`. The lock is held only for the O(1) push/drain of a
/// tiny `Vec` — never across a decode, a publish, or an `.await` (inv #1/#10);
/// the writer never blocks and the reader never awaits.
#[derive(Debug, Default)]
pub struct TransportMailbox {
    pending: Mutex<Vec<TransportVerb>>,
}

impl TransportMailbox {
    /// A fresh, empty mailbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Submit a verb (control-plane side). Bounded by construction (see the type
    /// docs): a state-collapsing verb conflates latest-wins; a targeted verb
    /// joins a bounded drop-oldest FIFO. Never blocks; never grows unbounded.
    pub fn submit(&self, verb: TransportVerb) {
        // Poisoned lock: the data plane must not panic — fall back to a no-op
        // (the thread keeps playing last-good; the operator can re-submit).
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if is_state_collapsing(&verb) {
            // Conflate: at most one state-collapsing verb pending.
            pending.retain(|v| !is_state_collapsing(v));
            pending.push(verb);
            return;
        }
        // Targeted verb: bounded FIFO, drop-oldest on overflow. Count the
        // targeted verbs already pending; if at the cap, evict the oldest one
        // (the first targeted verb in submission order) before pushing.
        let targeted = pending.iter().filter(|v| !is_state_collapsing(v)).count();
        if targeted >= MAX_TARGETED_PENDING {
            if let Some(pos) = pending.iter().position(|v| !is_state_collapsing(v)) {
                let dropped = pending.remove(pos);
                tracing::warn!(
                    ?dropped,
                    cap = MAX_TARGETED_PENDING,
                    "media transport mailbox full: dropped the oldest targeted verb (drop-oldest)"
                );
            }
        }
        pending.push(verb);
    }

    /// Drain all pending verbs (ingest-thread side), in submission order.
    /// Returns an empty `Vec` when nothing is pending. Never blocks beyond the
    /// O(1) swap.
    #[must_use]
    pub fn drain(&self) -> Vec<TransportVerb> {
        match self.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            // Poisoned: nothing to apply this tick (keep last-good).
            Err(_) => Vec::new(),
        }
    }
}

/// Whether a verb collapses transport *state* (so only the latest matters) vs
/// carrying a distinct target that must be preserved in order.
const fn is_state_collapsing(verb: &TransportVerb) -> bool {
    matches!(
        verb,
        TransportVerb::Play
            | TransportVerb::Vamp
            | TransportVerb::Pause
            | TransportVerb::Stop
            | TransportVerb::ArmExit
            | TransportVerb::TakeExit
            | TransportVerb::CancelExit
    )
}
