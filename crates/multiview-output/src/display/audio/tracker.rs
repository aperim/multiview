//! The sample-vs-scanout skew tracker: the bookkeeping behind the servo's slow
//! alignment term (DEV-B4 / display-out §5, the three-clock servo).
//!
//! The drain loop ([`super::sink`]) owns one tracker per PCM session and feeds
//! it two event streams: frames accepted by the device ([`on_written`]) and the
//! per-iteration flip-clock/delay observation ([`skew_input`], whose return
//! value is the `skew_ms` input to [`BufferServo::correction`]). Extracted from
//! the drain loop so the whole skew path is drivable in closed-loop simulations
//! over the real [`AudioFifo`](super::AudioFifo) + [`BufferServo`] +
//! [`AdaptiveResampler`](multiview_audio::AdaptiveResampler) — no hardware, no
//! threads.
//!
//! [`on_written`]: SkewTracker::on_written
//! [`skew_input`]: SkewTracker::skew_input
//! [`BufferServo`]: super::BufferServo
//! [`BufferServo::correction`]: super::BufferServo::correction

use multiview_audio::RatioPpm;

use super::servo::skew_ms;

/// Per-PCM-session skew bookkeeping: accumulates the frames the device
/// accepted and anchors them against the display flip clock to measure how far
/// the audio clock has run ahead of (positive) or behind (negative) scanout.
///
/// The current accounting counts **post-resample device frames** as written
/// (`on_written` ignores the applied ratio): the device consumes those at its
/// own crystal rate regardless of the resampler ratio, so the measurement is
/// the device clock vs the flip clock, not the *content* position. The anchor
/// subtracts only the *current* PCM delay, not the delay captured at anchor
/// time. An xrun drops the anchor (the device position jumps across a
/// recover); the next observation with a live flip clock re-anchors.
#[derive(Debug, Clone)]
pub struct SkewTracker {
    /// Total device frames accepted by the PCM this session.
    delivered: u64,
    /// `(flip ns, delivered frames)` captured at anchor time; [`None`] until
    /// the first observation after a write while the flip clock is live, and
    /// after any xrun.
    anchor: Option<(u64, u64)>,
}

impl Default for SkewTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl SkewTracker {
    /// A fresh, un-anchored tracker (one per PCM session).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            delivered: 0,
            anchor: None,
        }
    }

    /// Record `frames` accepted by the device at the currently-applied
    /// resampler ratio.
    ///
    /// The ratio parameter is deliberately unused by the current accounting
    /// (post-resample device frames are accumulated as-is).
    pub fn on_written(&mut self, frames: u64, _applied: RatioPpm) {
        self.delivered = self.delivered.saturating_add(frames);
    }

    /// The device position jumps across an xrun recover: drop the anchor so
    /// the next live observation re-anchors.
    pub fn on_xrun(&mut self) {
        self.anchor = None;
    }

    /// Whether the tracker currently holds a skew anchor (telemetry/tests).
    #[must_use]
    pub const fn is_anchored(&self) -> bool {
        self.anchor.is_some()
    }

    /// One per-iteration observation: the latest flip-clock value (`0` while
    /// no flip has landed / no flip clock exists), the PCM's current delay in
    /// device frames (`snd_pcm_delay`), the resampler ratio currently applied,
    /// and the negotiated device rate. Returns the skew (ms; positive = audio
    /// ahead of scanout) to feed the servo.
    ///
    /// Un-anchored: returns `0.0` and anchors `(flip_ns, delivered)` once at
    /// least one write has landed while the flip clock is live. Anchored: the
    /// skew is the device frames played since the anchor (delivered minus the
    /// anchor baseline minus the *current* delay) against the flip-clock span
    /// since the anchor. The applied-ratio parameter is deliberately unused by
    /// the current accounting.
    pub fn skew_input(
        &mut self,
        flip_ns: u64,
        delay_frames: Option<i64>,
        _applied: RatioPpm,
        rate: u32,
    ) -> f64 {
        if flip_ns == 0 {
            return 0.0;
        }
        match self.anchor {
            None => {
                if self.delivered > 0 {
                    self.anchor = Some((flip_ns, self.delivered));
                }
                0.0
            }
            Some((flip0, delivered0)) => {
                if flip_ns > flip0 {
                    let played = self
                        .delivered
                        .saturating_sub(delivered0)
                        .saturating_sub(delay_frames.map_or(0, |d| d.max(0).unsigned_abs()));
                    skew_ms(played, rate, flip_ns - flip0)
                } else {
                    0.0
                }
            }
        }
    }
}
