//! Node presentation discipline (DEV-C2, [ADR-0045] §1 / [ADR-M010] /
//! display-out §8): the **pull-side frame chooser** — a bounded
//! 2–3-frame presentation queue drained from the wait-free
//! [`mailbox`](super::mailbox), an exact-rational vblank predictor anchored on
//! KMS flip timestamps, and the pure "present the frame whose
//! `wall_at(pts) + link_offset` is closest to the predicted next vblank;
//! repeat-if-early, drop-if-late" decision.
//!
//! Everything here is **pure** and exact-integer (invariant #3 — never float
//! fps): the chooser, the predictor, and the queue take plain `i64`
//! nanoseconds and exact [`Rational`] refreshes, so the sink's whole
//! presentation behaviour is CI-proven hardware-free over the scripted
//! [`KmsBackend`](super::KmsBackend) + clock seam.
//!
//! ## Why this is invariant #1 / #10 safe by construction
//!
//! The node is a **pull-side consumer**: it samples the engine's latest canvas
//! through the wait-free mailbox and chooses which queued frame to scan out at
//! each vblank. Nothing here feeds back to the engine, paces the output clock,
//! or back-pressures any producer — the presentation queue is bounded
//! drop-oldest on the *pull* side of the mailbox seam, the engine-side publish
//! stays one wait-free overwrite, and a lost controller feed only stops epoch
//! updates (the node free-runs on the last epoch; see [`super::sink`]).
//!
//! [ADR-0045]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-0045.md
//! [ADR-M010]: https://github.com/aperim/multiview/blob/main/docs/decisions/ADR-M010.md

use multiview_core::time::{MediaTime, Rational};
use multiview_core::wallclock::WallClockRef;

/// The bounded presentation-queue depth (display-out §8: "decode into a small
/// queue (2–3 frames)"). Three frames is the upper bound the brief names: deep
/// enough to choose the nearest-to-vblank frame, shallow enough that a lost
/// feed never grows latency.
pub const PRESENT_QUEUE_DEPTH: usize = 3;

/// What the pull-side chooser decided for a vblank, given the queued frames'
/// deadlines and the predicted next vblank instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameChoice {
    /// Nothing is queued — present nothing (KMS keeps the current glass).
    Idle,
    /// The nearest queued frame is more than half a vblank period in the
    /// future: it belongs to the *next* vblank, so repeat the current
    /// framebuffer for free (KMS repeats it; no commit).
    RepeatEarly,
    /// Present the queued frame at `index` (the one whose deadline is nearest
    /// the predicted vblank). Every earlier queued frame is a late skip.
    Present {
        /// The chosen frame's position in the deadline slice.
        index: usize,
    },
}

/// Choose which queued frame to present at the predicted next vblank, given
/// each queued frame's **deadline** (`wall_at(pts) + link_offset`, in the same
/// clock domain as `predicted_vblank_ns`) and the vblank `period_ns`.
///
/// The rule (display-out §8):
///
/// * empty queue → [`FrameChoice::Idle`];
/// * a degenerate (`<= 0`) period carries no usable phase, so discipline is
///   impossible — present the **newest** frame (newest-wins keeps the glass
///   live);
/// * otherwise pick the frame whose deadline is **nearest** the predicted
///   vblank (ties prefer the newer/higher-indexed frame — never show older
///   content when newer is equally placed);
/// * if that nearest frame is still **more than half a period** in the future
///   (`2·(deadline − vblank) > period`) it belongs to the next vblank →
///   [`FrameChoice::RepeatEarly`]; otherwise present it (a late-only frame
///   still presents, catching up — the output never falters).
///
/// Pure exact-integer arithmetic (`i128` intermediates for the distance and
/// the half-period test) — never float.
#[must_use]
pub fn choose_frame(deadlines: &[i64], predicted_vblank_ns: i64, period_ns: i64) -> FrameChoice {
    let Some(last) = deadlines.len().checked_sub(1) else {
        return FrameChoice::Idle;
    };
    // A degenerate period gives no discipline: newest-wins keeps the glass live.
    if period_ns <= 0 {
        return FrameChoice::Present { index: last };
    }
    let vblank = i128::from(predicted_vblank_ns);
    // Argmin |deadline − vblank|, ties → the newer (higher) index. Iterating in
    // order and replacing on `<=` makes the *last* equal-distance frame win.
    let mut best_index = last;
    let mut best_distance = i128::MAX;
    for (index, &deadline) in deadlines.iter().enumerate() {
        let distance = (i128::from(deadline) - vblank).abs();
        if distance <= best_distance {
            best_distance = distance;
            best_index = index;
        }
    }
    // Repeat-if-early: the chosen frame is more than half a period beyond the
    // vblank (`2·(d − V) > period`). The signed delta — not the absolute
    // distance — so a late frame (negative delta) never trips the gate.
    let Some(&chosen_deadline) = deadlines.get(best_index) else {
        return FrameChoice::Present { index: last };
    };
    let signed_ahead = i128::from(chosen_deadline) - vblank;
    if signed_ahead.saturating_mul(2) > i128::from(period_ns) {
        return FrameChoice::RepeatEarly;
    }
    FrameChoice::Present { index: best_index }
}

/// The flip-anchored vblank predictor: an exact-rational refresh period plus
/// the phase anchored on the last KMS page-flip timestamp.
///
/// The kernel delivers a precise monotonic-clock timestamp per page-flip
/// completion; the predictor keeps the **measured** phase (each flip
/// re-anchors the grid, so scanout drift never accumulates) and projects the
/// next vblank as the first grid instant strictly after a queried "now". All
/// arithmetic is exact integer ns (invariant #3): the period is one exact
/// [`rescale`] of the refresh rational into nanoseconds — never float fps.
#[derive(Debug, Clone, Copy)]
pub struct VblankPredictor {
    /// The exact vblank period in ns (`0` for a degenerate refresh).
    period_ns: i64,
    /// The last flip timestamp the grid is anchored on (`None` until the first
    /// flip — before any flip there is no phase to predict from).
    anchor_ns: Option<i64>,
}

impl VblankPredictor {
    /// Build a predictor for a `refresh` rate (Hz, exact rational). The period
    /// is `1/refresh` seconds expressed exactly in nanoseconds; a degenerate
    /// (`<= 0`) refresh yields a zero period (the predictor then never
    /// predicts).
    #[must_use]
    pub fn new(refresh: Rational) -> Self {
        // period = 1 frame at `refresh` fps, in ns. A frame spans
        // `refresh.den/refresh.num` seconds; rescale that into the 1 ns
        // timebase. `MediaTime::from_tick` is exactly this (tick 1 at the
        // cadence), so reuse it — one exact `rescale`, never float.
        let period_ns = if refresh.is_valid() && refresh.num > 0 {
            MediaTime::from_tick(1, refresh).as_nanos()
        } else {
            0
        };
        Self {
            period_ns,
            anchor_ns: None,
        }
    }

    /// The exact vblank period in nanoseconds (`0` for a degenerate refresh).
    #[must_use]
    pub const fn period_ns(&self) -> i64 {
        self.period_ns
    }

    /// Re-anchor the grid on a measured KMS flip timestamp (monotonic ns). The
    /// phase is taken from the kernel's actual flip instant, so prediction
    /// error never accumulates across flips.
    pub fn on_flip(&mut self, flip_ns: i64) {
        self.anchor_ns = Some(flip_ns);
    }

    /// Predict the next vblank instant (monotonic ns) strictly after `now_ns`,
    /// or [`None`] when there is no usable phase yet (no flip anchor) or the
    /// refresh is degenerate (zero period).
    ///
    /// The grid is `anchor + k·period`; the prediction is the first grid
    /// instant strictly greater than `now_ns`. When `now` sits exactly on a
    /// grid instant the prediction is the *following* one (a vblank strictly in
    /// the future). Exact-integer (`i128`) division — never float.
    #[must_use]
    pub fn predicted_next_ns(&self, now_ns: i64) -> Option<i64> {
        if self.period_ns <= 0 {
            return None;
        }
        let anchor = self.anchor_ns?;
        let period = i128::from(self.period_ns);
        // k = floor((now − anchor)/period) + 1 → the first grid instant > now.
        let delta = i128::from(now_ns) - i128::from(anchor);
        let k = delta.div_euclid(period) + 1;
        let next = i128::from(anchor) + k.saturating_mul(period);
        i64::try_from(next).ok()
    }
}

/// One bounded, drop-oldest pull-side presentation queue entry: the frame, the
/// mailbox sequence it carried, and its presentation timestamp (output-PTS ns).
#[derive(Debug, Clone, Copy)]
struct QueueEntry<F> {
    frame: F,
    seq: u64,
    pts_ns: i64,
}

/// The bounded pull-side presentation queue ([`PRESENT_QUEUE_DEPTH`] frames):
/// the node drains the wait-free mailbox into this small queue and the chooser
/// picks which entry to scan out at each vblank.
///
/// Bounded drop-oldest (invariant #9/#10: queues drop, never grow): a push
/// past the depth evicts the **oldest** entry (newest content wins) and reports
/// the overflow so the sink can count it. This is the *pull* side of the
/// mailbox seam — the engine-side publish is untouched and stays wait-free.
#[derive(Debug)]
pub struct PresentQueue<F> {
    entries: std::collections::VecDeque<QueueEntry<F>>,
}

impl<F> Default for PresentQueue<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> PresentQueue<F> {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: std::collections::VecDeque::with_capacity(PRESENT_QUEUE_DEPTH),
        }
    }

    /// Whether the queue holds no frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The number of queued frames (`0..=`[`PRESENT_QUEUE_DEPTH`]).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Push a frame (with its mailbox `seq` and presentation `pts_ns`) onto the
    /// back. Returns `true` when the queue was full and the **oldest** frame was
    /// dropped to make room (an overflow the caller counts), `false` otherwise.
    pub fn push(&mut self, frame: F, seq: u64, pts_ns: i64) -> bool {
        let overflow = self.entries.len() >= PRESENT_QUEUE_DEPTH;
        if overflow {
            self.entries.pop_front();
        }
        self.entries.push_back(QueueEntry { frame, seq, pts_ns });
        overflow
    }

    /// Borrow the `index`-th entry as `(frame, seq, pts_ns)`, or [`None`] when
    /// out of range.
    #[must_use]
    pub fn entry(&self, index: usize) -> Option<(&F, u64, i64)> {
        self.entries.get(index).map(|e| (&e.frame, e.seq, e.pts_ns))
    }

    /// The mailbox sequence of the newest queued frame, or `0` when empty (the
    /// drain uses this to push only genuinely-newer mailbox frames).
    #[must_use]
    pub fn newest_seq(&self) -> u64 {
        self.entries.back().map_or(0, |e| e.seq)
    }

    /// Consume the queue **through** `index` (inclusive): every entry before
    /// `index` is a late skip (dropped, never shown) and the entry at `index`
    /// is removed (it is being presented). Returns the number of late skips.
    /// Out-of-range indices clamp to the queue length (no-op past the end).
    pub fn pop_through(&mut self, index: usize) -> usize {
        let skips = index.min(self.entries.len());
        for _ in 0..skips {
            self.entries.pop_front();
        }
        // Remove the chosen frame itself (now at the front).
        let _ = self.entries.pop_front();
        skips
    }

    /// The per-entry **deadlines** (`wall_at(pts) + link_offset_ns`) in the
    /// epoch's wall-clock domain, in queue order. The chooser compares these
    /// against the predicted vblank (converted into the same domain). Exact
    /// integer ns — never float.
    #[must_use]
    pub fn deadlines(&self, epoch: WallClockRef, link_offset_ns: i64) -> Vec<i64> {
        self.entries
            .iter()
            .map(|e| epoch.wall_at(e.pts_ns).saturating_add(link_offset_ns))
            .collect()
    }
}

/// A monotonic/wall clock pair the presentation loop samples: `now_pair`
/// returns `(monotonic_ns, wall_ns)` read close together, so the loop can map
/// the predicted vblank (a monotonic KMS-flip-domain instant) into the epoch's
/// wall-clock domain and back.
///
/// The real node implementation reads `CLOCK_MONOTONIC` (the KMS flip-timestamp
/// domain) and `CLOCK_REALTIME` (the disciplined wall domain the epoch is
/// defined in); tests inject a scripted pair. Taking `&mut self` lets a real
/// clock cache fds without interior mutability.
pub trait PresentationClock: Send {
    /// One coherent `(monotonic_ns, wall_ns)` reading.
    fn now_pair(&mut self) -> (i64, i64);
}

/// Convert an exact `link_offset_ms` (milliseconds, the node config knob) into
/// integer nanoseconds for the deadline math (exact — never float).
#[must_use]
pub fn link_offset_ms_to_ns(link_offset_ms: u32) -> i64 {
    i64::from(link_offset_ms).saturating_mul(1_000_000)
}

/// The per-deployment presentation plan a node sink runs with (DEV-C2): the
/// shared outbound presentation [`epoch`](crate::SharedEpoch) (read-only — the
/// node consumes it, never writes), the fixed receiver-side link offset
/// (ns), and the monotonic/wall [`PresentationClock`] pair the loop samples.
///
/// A sink with no plan runs the DEV-B1 undisciplined latest-wins loop; a sink
/// with a plan runs the pull-side frame chooser, falling back to latest-wins
/// whenever the epoch is unpublished or no flip has anchored the predictor yet
/// (the output never waits for timing).
pub struct PresentationPlan {
    /// The shared outbound presentation epoch (output-PTS ns → wall ns). The
    /// node reads it; a lost controller feed only stops updates and the node
    /// free-runs drift-bounded on the last map (display-out §8 degradation).
    pub epoch: crate::SharedEpoch,
    /// The fixed per-deployment receiver-side delay (ns) added to
    /// `wall_at(pts)` (AES67 link-offset semantics applied to video —
    /// uniformity across nodes matters, not smallness; ADR-M010).
    pub link_offset_ns: i64,
    /// The monotonic/wall clock pair the loop samples to bridge the KMS flip
    /// (monotonic) domain and the epoch (wall) domain.
    pub clock: Box<dyn PresentationClock>,
}

impl std::fmt::Debug for PresentationPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PresentationPlan")
            .field("epoch", &self.epoch)
            .field("link_offset_ns", &self.link_offset_ns)
            .field("clock", &"<dyn PresentationClock>")
            .finish()
    }
}

/// The **real** node presentation clock (feature `display-kms`): reads the OS
/// `CLOCK_MONOTONIC` (the kernel domain DRM page-flip timestamps are stamped in)
/// and `CLOCK_REALTIME` (the disciplined wall domain the epoch is defined in) as
/// one coherent pair, via the safe `rustix::time::clock_gettime` (no `unsafe` —
/// the crate stays `forbid(unsafe_code)`).
///
/// The two reads are taken back-to-back; the few-ns gap between them is far
/// below the sub-millisecond presentation-discipline tolerance, so the sampled
/// `wall − mono` offset is exact for the deadline bridge.
#[cfg(feature = "display-kms")]
#[derive(Debug, Default)]
pub struct RealtimePresentationClock;

#[cfg(feature = "display-kms")]
impl RealtimePresentationClock {
    /// A new real clock (stateless — each read calls `clock_gettime`).
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// One `clock_gettime` reading as integer nanoseconds.
    fn read_ns(clock: rustix::time::ClockId) -> i64 {
        let ts = rustix::time::clock_gettime(clock);
        ts.tv_sec
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(ts.tv_nsec))
    }
}

#[cfg(feature = "display-kms")]
impl PresentationClock for RealtimePresentationClock {
    fn now_pair(&mut self) -> (i64, i64) {
        let mono = Self::read_ns(rustix::time::ClockId::Monotonic);
        let wall = Self::read_ns(rustix::time::ClockId::Realtime);
        (mono, wall)
    }
}
