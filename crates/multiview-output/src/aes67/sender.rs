//! The AES67 / ST 2110-30 **sender core** (ADR-0033 §1/§7): a bounded
//! drop-oldest program-audio sink that packetizes the mixed program bus to
//! continuous marker=0 RTP.
//!
//! The engine's off-hot-path bake consumer pushes baked program
//! [`AudioBlock`]s in via [`Aes67Sender::push`] (a bounded, drop-oldest copy
//! that never blocks — invariant #10); an independent send timer drains one
//! packet-time's worth of samples per tick via [`Aes67Sender::next_packet`],
//! silence-filling any underrun so the stream is **continuous** (one valid
//! marker=0 packet per tick forever — invariant #1). The two run on independent
//! clocks and the sender can never back-pressure or re-pace the engine.
//!
//! Timestamps advance by **sample-groups** (`+frames_per_packet` per packet,
//! e.g. `+48` at 1 ms / 48 kHz), never by bytes or channels; the sequence
//! advances `+1` per packet; the SSRC is a constant chosen at construction.
//! (The PTP-anchored *absolute* send timestamp — ADR-0033 §4 — is a flagged
//! hardware follow-on; this software sender uses a free-running counter, which
//! preserves the continuous cadence and never paces the engine.)

use multiview_audio::AudioBlock;

use crate::display::audio::AudioFifo;

use super::packet::{build_rtp_header, encode_pcm, PcmDepth, RTP_FIXED_HEADER_LEN};

/// Default send FIFO depth in frames: 100 ms at 48 kHz. Bounds the decoupling
/// buffer between the engine bake and the send timer; on overflow the oldest
/// frames are dropped so a slow send timer never grows memory (invariant #10).
pub const DEFAULT_SEND_CAPACITY_FRAMES: usize = 4_800;

/// A bounded drop-oldest AES67 / ST 2110-30 program-audio sender.
///
/// Construct with [`Aes67Sender::new`], feed baked program blocks with
/// [`push`](Aes67Sender::push), and drain one continuous RTP packet per
/// packet-time tick with [`next_packet`](Aes67Sender::next_packet).
#[derive(Debug)]
pub struct Aes67Sender {
    fifo: AudioFifo,
    channels: usize,
    depth: PcmDepth,
    payload_type: u8,
    ssrc: u32,
    /// Audio frames (samples per channel) emitted per packet (`ptime` worth).
    frames_per_packet: usize,
    /// The RTP timestamp increment per packet (`= frames_per_packet`, precomputed
    /// as `u32` so the hot path never casts).
    timestamp_increment: u32,
    sequence: u16,
    timestamp: u32,
    /// Reusable drain buffer (rule 22: no per-packet interleaved allocation).
    scratch: Vec<f32>,
}

impl Aes67Sender {
    /// Build a sender for `channels`-channel interleaved audio at `depth`,
    /// emitting `frames_per_packet` frames per packet on RTP payload type
    /// `payload_type` with the constant `ssrc`, buffering up to
    /// `capacity_frames` frames (clamped to at least one packet).
    #[must_use]
    pub fn new(
        channels: usize,
        depth: PcmDepth,
        payload_type: u8,
        ssrc: u32,
        frames_per_packet: usize,
        capacity_frames: usize,
    ) -> Self {
        let channels = channels.max(1);
        let frames_per_packet = frames_per_packet.max(1);
        let capacity_frames = capacity_frames.max(frames_per_packet);
        Self {
            fifo: AudioFifo::new(capacity_frames, channels),
            channels,
            depth,
            payload_type,
            ssrc,
            frames_per_packet,
            timestamp_increment: u32::try_from(frames_per_packet).unwrap_or(u32::MAX),
            sequence: 0,
            timestamp: 0,
            scratch: Vec::new(),
        }
    }

    /// Push one baked program block into the send FIFO — a bounded, drop-oldest
    /// copy that never blocks (invariant #10). A block whose channel count does
    /// not match the sender's is dropped rather than mis-interleaved (the
    /// pipeline constructs the sender with the program bus's channel count, so
    /// this is a defensive guard, not the steady path).
    pub fn push(&mut self, block: &AudioBlock) {
        if block.format().channel_count() == self.channels {
            self.fifo.push(block.interleaved());
        }
    }

    /// Build the next continuous RTP packet: header + one packet-time of PCM.
    ///
    /// Drains `frames_per_packet` frames of real audio, silence-filling any
    /// underrun so the stream never gaps or stalls (invariant #1), encodes them
    /// to big-endian L16/L24, prepends the 12-byte RTP header (marker=0), and
    /// advances the sequence (`+1`) and timestamp (`+frames_per_packet`). Always
    /// returns a full packet, whatever the FIFO fill — the RTP media clock is
    /// driven by the send timer, never by the input feed.
    pub fn next_packet(&mut self) -> Vec<u8> {
        let want_samples = self.frames_per_packet.saturating_mul(self.channels);
        self.scratch.resize(want_samples, 0.0);
        // `pop_into` writes exactly `want_samples` values: real samples where the
        // FIFO has them, silence (0.0) for any underrun tail.
        let _real = self.fifo.pop_into(&mut self.scratch);

        let payload = encode_pcm(&self.scratch, self.depth);
        let header = build_rtp_header(self.payload_type, self.sequence, self.timestamp, self.ssrc);

        let mut packet = Vec::with_capacity(RTP_FIXED_HEADER_LEN.saturating_add(payload.len()));
        packet.extend_from_slice(&header);
        packet.extend_from_slice(&payload);

        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.timestamp_increment);
        packet
    }

    /// Current FIFO fill in frames (per channel).
    #[must_use]
    pub fn fill_frames(&self) -> usize {
        self.fifo.fill_frames()
    }

    /// Total frames dropped to drop-oldest overflow since construction
    /// (telemetry / test observability).
    #[must_use]
    pub fn dropped_frames(&self) -> u64 {
        self.fifo.dropped_frames()
    }

    /// The next RTP sequence number to be emitted.
    #[must_use]
    pub const fn sequence(&self) -> u16 {
        self.sequence
    }

    /// The next RTP timestamp to be emitted.
    #[must_use]
    pub const fn timestamp(&self) -> u32 {
        self.timestamp
    }

    /// The audio frames emitted per packet.
    #[must_use]
    pub const fn frames_per_packet(&self) -> usize {
        self.frames_per_packet
    }
}
