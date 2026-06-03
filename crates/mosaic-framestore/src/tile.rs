//! The per-tile last-good-frame store: lock-free slot + failure-ladder state.
//!
//! [`TileStore<T>`] ties together the lock-free [`LatestSlot`] (the
//! last-good-frame cell) and the [`classify`] failure-ladder policy. It is the
//! concrete realization of invariant #2 for a single tile:
//!
//! * **Writers** (a decoder) call [`TileStore::publish`] with the frame and the
//!   timeline instant it arrived. This is non-blocking and newest-wins.
//! * **Readers** (the compositor, on each output tick) call [`TileStore::read`]
//!   with the current instant. They never block and always get a definite
//!   answer: a fresh frame, the *held* last-good frame, or an explicit
//!   `NoSignal` indicator — never a stall.
//!
//! Time is **injected**: every method that needs "now" takes a [`MediaTime`],
//! so the whole state ladder is deterministically testable with no real clock
//! and no sleeps.
use core::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use mosaic_core::time::MediaTime;
use mosaic_core::traits::SourceState;

use crate::latest::LatestSlot;
use crate::state::{classify, TileThresholds};

/// Sentinel stored in [`TileStore::last_frame_at_ns`] meaning "no frame has
/// ever been published". `i64::MIN` can never be a real publish instant
/// (timelines are non-negative and bounded well away from the `i64` extremes),
/// so it unambiguously encodes the not-yet-published state without a separate
/// flag or an `Option` allocation.
const NEVER_PUBLISHED: i64 = i64::MIN;

/// The outcome of reading a tile on an output tick.
///
/// A reader always receives one of these — the compositor never blocks waiting
/// for a tile (invariant #2 / #1). `#[non_exhaustive]` so new render hints can
/// be added without breaking downstream `match`es.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TileRead<T> {
    /// A fresh frame within the `hold` window — composite it directly.
    ///
    /// Carries the tile's current [`SourceState`] (always [`SourceState::Live`]
    /// for this variant) for uniform telemetry.
    Fresh {
        /// The frame to composite.
        frame: Arc<T>,
    },
    /// No fresh frame, but a last-good frame is held — composite it (the tile
    /// is `STALE` or `RECONNECTING`), letting the rest of the canvas continue.
    Held {
        /// The held last-good frame.
        frame: Arc<T>,
        /// The degraded state the tile is now in (`Stale`/`Reconnecting`, or
        /// `NoSignal` if a frame was ever seen but the no-signal threshold has
        /// elapsed).
        state: SourceState,
    },
    /// No usable signal: the tile has either never received a frame, or has
    /// been starved past the `nosignal` threshold and the policy is to stop
    /// holding. The compositor should render a placeholder/slate card.
    NoSignal,
}

impl<T> TileRead<T> {
    /// The [`SourceState`] this read corresponds to.
    #[must_use]
    pub fn state(&self) -> SourceState {
        match self {
            Self::Fresh { .. } => SourceState::Live,
            Self::Held { state, .. } => *state,
            Self::NoSignal => SourceState::NoSignal,
        }
    }

    /// The held/fresh frame, if any (`None` for [`TileRead::NoSignal`]).
    #[must_use]
    pub fn frame(&self) -> Option<&Arc<T>> {
        match self {
            Self::Fresh { frame } | Self::Held { frame, .. } => Some(frame),
            Self::NoSignal => None,
        }
    }
}

/// Whether a tile should keep holding its last-good frame once it crosses the
/// `nosignal` threshold, or hand back a `NoSignal` indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum NoSignalPolicy {
    /// Stop holding once `NO_SIGNAL` is reached: [`TileStore::read`] returns
    /// [`TileRead::NoSignal`] so the compositor draws a slate card. This is the
    /// broadcast-correct default (a frozen frame for minutes is misleading).
    #[default]
    Slate,
    /// Keep holding the last-good frame even in `NO_SIGNAL` (still reports the
    /// `NoSignal` *state* via [`TileRead::Held`]). Useful for tiles where a
    /// frozen last frame is preferable to a slate.
    HoldForever,
}

/// A per-tile last-good-frame store with an attached failure-ladder state
/// machine.
///
/// Generic over the stored payload `T` (a backend frame handle, a decoded
/// surface wrapper, …). The store itself only ever holds `Arc<T>`, so cloning a
/// read is cheap and tear-free.
#[derive(Debug)]
pub struct TileStore<T> {
    id: Arc<str>,
    slot: LatestSlot<T>,
    thresholds: TileThresholds,
    policy: NoSignalPolicy,
    /// Timeline instant of the most recent published frame, as raw nanoseconds,
    /// or [`NEVER_PUBLISHED`] until the first frame arrives. A plain atomic
    /// (not an `Arc` cell) so a reader observes it lock-free with no allocation
    /// and no extra reclamation machinery alongside the frame slot.
    last_frame_at_ns: AtomicI64,
}

impl<T> TileStore<T> {
    /// Create an empty tile store with the given id, thresholds, and no-signal
    /// policy. Until the first [`publish`](TileStore::publish), the tile is in
    /// [`SourceState::NoSignal`].
    #[must_use]
    pub fn new(
        id: impl Into<Arc<str>>,
        thresholds: TileThresholds,
        policy: NoSignalPolicy,
    ) -> Self {
        Self {
            id: id.into(),
            slot: LatestSlot::new(),
            thresholds,
            policy,
            last_frame_at_ns: AtomicI64::new(NEVER_PUBLISHED),
        }
    }

    /// Create a tile store with default thresholds and the [`NoSignalPolicy::Slate`]
    /// default.
    #[must_use]
    pub fn with_defaults(id: impl Into<Arc<str>>) -> Self {
        Self::new(id, TileThresholds::default(), NoSignalPolicy::default())
    }

    /// The stable tile/source identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The configured thresholds.
    #[must_use]
    pub fn thresholds(&self) -> TileThresholds {
        self.thresholds
    }

    /// Publish a fresh frame that arrived at timeline instant `at`.
    ///
    /// Non-blocking, newest-wins: any previously held frame is dropped. Records
    /// `at` as the last-fresh-frame instant, which resets the failure ladder to
    /// [`SourceState::Live`] (as observed by a subsequent [`read`](TileStore::read)
    /// at `now >= at` with `now - at < hold`).
    ///
    /// Returns the slot sequence number assigned to this frame.
    pub fn publish(&self, frame: T, at: MediaTime) -> u64 {
        self.publish_arc(Arc::new(frame), at)
    }

    /// As [`publish`](TileStore::publish), for a frame already wrapped in an
    /// [`Arc`].
    pub fn publish_arc(&self, frame: Arc<T>, at: MediaTime) -> u64 {
        let seq = self.slot.publish_arc(frame);
        // Record the arrival instant after the frame is visible in the slot, so
        // a reader that observes a fresh `last_frame_at_ns` is guaranteed to
        // also see (at least) this frame in the slot — never a newer timestamp
        // with an older/missing frame.
        self.last_frame_at_ns
            .store(at.as_nanos(), Ordering::Release);
        seq
    }

    /// The elapsed time since the last fresh frame as of `now`, or [`None`] if
    /// no frame has ever been published.
    ///
    /// Saturating: a `now` earlier than the last frame yields `0` (the
    /// monotonic-guard case), never a negative duration.
    #[must_use]
    pub fn elapsed_since_frame(&self, now: MediaTime) -> Option<MediaTime> {
        let last_ns = self.last_frame_at_ns.load(Ordering::Acquire);
        if last_ns == NEVER_PUBLISHED {
            return None;
        }
        Some(now.saturating_sub(MediaTime::from_nanos(last_ns)))
    }

    /// The tile's [`SourceState`] as of `now`.
    ///
    /// Pure function of `now`, the last-frame instant, and the thresholds. A
    /// tile that has never received a frame is [`SourceState::NoSignal`].
    #[must_use]
    pub fn state(&self, now: MediaTime) -> SourceState {
        match self.elapsed_since_frame(now) {
            Some(elapsed) => classify(elapsed, self.thresholds),
            None => SourceState::NoSignal,
        }
    }

    /// Read the tile on an output tick at instant `now`.
    ///
    /// Never blocks. Returns:
    /// * [`TileRead::Fresh`] when a frame is held and the tile is `LIVE`;
    /// * [`TileRead::Held`] when a frame is held but the tile is degraded
    ///   (`STALE`/`RECONNECTING`, or `NO_SIGNAL` under
    ///   [`NoSignalPolicy::HoldForever`]);
    /// * [`TileRead::NoSignal`] when no frame is held, or the tile is
    ///   `NO_SIGNAL` under [`NoSignalPolicy::Slate`].
    #[must_use]
    pub fn read(&self, now: MediaTime) -> TileRead<T> {
        let Some(frame) = self.slot.load() else {
            // Nothing ever published (or explicitly cleared).
            return TileRead::NoSignal;
        };
        match self.state(now) {
            SourceState::Live => TileRead::Fresh { frame },
            state @ (SourceState::Stale | SourceState::Reconnecting) => {
                TileRead::Held { frame, state }
            }
            SourceState::NoSignal => match self.policy {
                NoSignalPolicy::HoldForever => TileRead::Held {
                    frame,
                    state: SourceState::NoSignal,
                },
                NoSignalPolicy::Slate => TileRead::NoSignal,
            },
            // `SourceState` is `#[non_exhaustive]`; treat any future state
            // conservatively as "no usable fresh signal".
            _ => match self.policy {
                NoSignalPolicy::HoldForever => TileRead::Held {
                    frame,
                    state: self.state(now),
                },
                NoSignalPolicy::Slate => TileRead::NoSignal,
            },
        }
    }

    /// The most recent slot sequence number (`0` if nothing published).
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.slot.sequence()
    }

    /// Borrow the underlying lock-free slot (e.g. to share a reader handle).
    #[must_use]
    pub fn slot(&self) -> &LatestSlot<T> {
        &self.slot
    }
}
