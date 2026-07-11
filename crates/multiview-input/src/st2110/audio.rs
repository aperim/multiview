//! ST 2110-30 / AES67 PCM-audio **receive producer** (ADR-0033 §3, ADR-T013).
//!
//! The audio analogue of the video
//! [`St2110Producer`](super::transport::St2110Producer): it pulls RTP packet
//! units from a [`PacketSource`](super::transport::PacketSource), depacketizes
//! each with the pure [`V30Payload`] parser, converts the big-endian L16/L24
//! samples to the canonical interleaved `f32`, and yields an [`Aes67AudioFrame`]
//! carrying the **verbatim** 32-bit RTP media timestamp + SSRC + a
//! sequence-gap discontinuity flag.
//!
//! It deliberately does **not** touch the `AudioStore`: `multiview-input` does
//! not depend on `multiview-audio`. The caller (the CLI pipeline) feeds the
//! yielded frame's `(raw_timestamp, ssrc, discontinuity)` through the shared
//! [`RtpAudioRebaser`](crate::rtp_audio) — the one ADR-T013 rebase contract the
//! WebRTC-Opus path also uses — onto an absolute `AudioStore` frame index and
//! `publish_at`s the `f32` block there. Splitting the depacketize (here) from
//! the rebase+publish (the caller) keeps this crate decoupled from the store
//! exactly as the WebRTC audio path is.
//!
//! ## Isolation (invariants #1 / #10)
//!
//! [`Aes67AudioProducer::next_audio`] does **non-blocking pulls only** from the
//! source and never paces the output clock: an empty source yields `Ok(None)`
//! (the caller re-polls next tick) and a malformed payload is skipped, never
//! faulted. The stream is *sampled*, never *pacing* — a stalled or absent RTP
//! source simply stops yielding and the store rides silence-fill.

use super::v30::{Aes3Format, V30Payload};

/// Convert one depacketized ST 2110-30 payload to interleaved canonical `f32`
/// (frame-major, length `group_count * channels`).
///
/// The exact inverse-scale of the [`Aes67Packetizer`](super::packetize::Aes67Packetizer)
/// encode: **L16** divides by `32768` (`2^15`) and **L24** by `8388608`
/// (`2^23`). The encode scales by `32767` / `8388607` to keep full-scale
/// positive from wrapping to the most-negative code; that ±1-LSB asymmetry is
/// standard AES67 and lossless in the decode direction (a decoded sample lands
/// in `[-1.0, 1.0)`). Never panics and never `as`-casts a raw sample directly.
#[must_use]
pub fn pcm_to_f32(payload: &V30Payload<'_>) -> Vec<f32> {
    let channels = usize::from(payload.format.channels);
    let groups = payload.group_count();
    // RED scaffold (ADR-0033 §3, replaced in the GREEN commit): the right shape
    // but silence — the real big-endian L16/L24 -> f32 conversion lands next.
    vec![0.0_f32; groups.saturating_mul(channels)]
}

/// One depacketized ST 2110-30 / AES67 audio unit yielded by
/// [`Aes67AudioProducer`].
///
/// Carries the fields the shared [`RtpAudioRebaser`](crate::rtp_audio) consumes
/// (`raw_timestamp`, `ssrc`, `discontinuity`) plus the decoded interleaved
/// `f32` block the caller places on the `AudioStore` at the rebased frame index.
#[derive(Debug, Clone, PartialEq)]
pub struct Aes67AudioFrame {
    /// The verbatim 32-bit RTP media timestamp (audio sample-rate ticks — e.g.
    /// 48 kHz for Class A, advancing by sample-groups per packet). The caller
    /// rebases this via [`RtpAudioRebaser`](crate::rtp_audio).
    pub raw_timestamp: u32,
    /// The RTP synchronization source; a change re-anchors the rebaser.
    pub ssrc: u32,
    /// Whether a sequence gap was observed before this unit (a lost packet the
    /// rebaser treats as a discontinuity).
    pub discontinuity: bool,
    /// The PCM format (channel count + depth) the payload was parsed against.
    pub format: Aes3Format,
    /// Interleaved canonical `f32` samples (frame-major), length a whole
    /// multiple of the channel count.
    pub samples: Vec<f32>,
}

#[cfg(feature = "st2110")]
use crate::error::Result;
#[cfg(feature = "st2110")]
use crate::st2110::transport::PacketSource;

/// A receive producer over a [`PacketSource`](super::transport::PacketSource)
/// that yields [`Aes67AudioFrame`]s for the ADR-T013 rebase seam.
///
/// The audio IN-2 bridge for ST 2110-30 / AES67: non-blocking pulls only, never
/// paces the output clock, and reuses the corrected bounded drop-oldest
/// [`ChannelPacketSource`](super::transport::ChannelPacketSource) for the live
/// socket path (invariants #1 / #10).
#[cfg(feature = "st2110")]
pub struct Aes67AudioProducer {
    source: Box<dyn PacketSource + Send>,
    format: Aes3Format,
    /// The highest in-stream RTP sequence accepted so far (for gap detection).
    last_sequence: Option<u16>,
}

#[cfg(feature = "st2110")]
impl core::fmt::Debug for Aes67AudioProducer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // `Box<dyn PacketSource>` is not `Debug` (like the video St2110Producer).
        f.debug_struct("Aes67AudioProducer")
            .field("format", &self.format)
            .field("last_sequence", &self.last_sequence)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "st2110")]
impl Aes67AudioProducer {
    /// Build a producer over an application-supplied packet source, decoding
    /// against the SDP-negotiated [`Aes3Format`].
    #[must_use]
    pub fn new(source: Box<dyn PacketSource + Send>, format: Aes3Format) -> Self {
        Self {
            source,
            format,
            last_sequence: None,
        }
    }

    /// The PCM format this producer decodes against.
    #[must_use]
    pub const fn format(&self) -> Aes3Format {
        self.format
    }

    /// Pull the next depacketized audio unit, or `Ok(None)` when nothing is
    /// ready this tick.
    ///
    /// Non-blocking: an empty source yields `Ok(None)` (the caller re-polls next
    /// tick) and never blocks the data plane (invariant #1). A malformed payload
    /// (not a whole number of sample groups for the configured format) is
    /// **skipped**, never faulted — a single bad datagram must not stall the
    /// stream (invariants #1 / #2). A gap against the last accepted RTP sequence
    /// sets [`Aes67AudioFrame::discontinuity`].
    ///
    /// # Errors
    ///
    /// Propagates a source fault (a socket error surfaced by the underlying
    /// [`PacketSource`](super::transport::PacketSource)); the supervisor applies
    /// reconnect backoff rather than crashing the engine.
    pub fn next_audio(&mut self) -> Result<Option<Aes67AudioFrame>> {
        loop {
            let Some(packet) = self.source.poll_packet()? else {
                return Ok(None);
            };
            // A malformed / partial-group payload is dropped, not faulted.
            let Ok(payload) = V30Payload::parse(&packet.payload, self.format) else {
                continue;
            };
            let discontinuity = self.observe_sequence(packet.sequence);
            let samples = pcm_to_f32(&payload);
            return Ok(Some(Aes67AudioFrame {
                raw_timestamp: packet.timestamp,
                ssrc: packet.ssrc,
                discontinuity,
                format: self.format,
                samples,
            }));
        }
    }

    /// Update the sequence watermark and report whether a forward gap (a lost
    /// packet) was observed. A stale reordered packet does not move the
    /// watermark, so a later in-order packet still detects its own gap.
    fn observe_sequence(&mut self, sequence: u16) -> bool {
        let gap = self.last_sequence.is_some_and(|prev| {
            sequence != prev.wrapping_add(1) && crate::st2110::rtp::seq_after(prev, sequence)
        });
        let is_stale = self
            .last_sequence
            .is_some_and(|prev| !crate::st2110::rtp::seq_after(prev, sequence));
        if !is_stale {
            self.last_sequence = Some(sequence);
        }
        gap
    }
}
