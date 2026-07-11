//! AES67 / ST 2110-30 send-side wire framing (ADR-0033 §1): the `f32` → L16/L24
//! big-endian PCM payload encoder and the 12-byte RTP fixed-header builder.
//!
//! The **canonical send packetizer lives here in `multiview-output`** (ADR-0033
//! §1), not in `multiview-input`: the two crates stay decoupled (input owns the
//! depacketize/parse side, output owns the packetize/build side), exactly like
//! the TSL protocol's independent per-crate codecs. A dev-dependency round-trip
//! test pins this encoder byte-identical to `multiview-input`'s `V30Payload`
//! decoder — encode here, decode there, assert equality.
//!
//! ## The encode footgun (RFC 3190 / AES67)
//!
//! - **L16:** clamp to `[-1.0, 1.0]`, scale by `32767`, round, emit `i16`
//!   big-endian.
//! - **L24:** clamp, scale by **`8388607`** (`2^23 - 1`, **not** `2^23`), round,
//!   emit the high three bytes of `(v << 8)` big-endian — the exact layout
//!   `multiview-input::st2110::v30` sign-extends. Scaling by `2^23` would wrap a
//!   full-scale-positive sample to the most-negative code (an audible click).

/// PCM sample depth for an AES67 / ST 2110-30 stream (Class A: L16 or L24).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PcmDepth {
    /// 16-bit signed PCM ("L16"), 2 bytes per sample.
    L16,
    /// 24-bit signed PCM ("L24"), 3 bytes per sample.
    L24,
}

impl PcmDepth {
    /// The on-wire byte width of one sample at this depth.
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            PcmDepth::L16 => 2,
            PcmDepth::L24 => 3,
        }
    }
}

/// The RTP version this sender emits (RFC 3550 fixes it at `2`).
pub const RTP_VERSION: u8 = 2;

/// The RTP fixed-header length in bytes (no CSRC list, no extension).
pub const RTP_FIXED_HEADER_LEN: usize = 12;

/// Build the 12-byte RTP fixed header for one AES67 audio packet.
///
/// Version 2, **no** padding / extension / CSRC, **marker = 0 on every packet**
/// (Multiview is a continuous, non-silence-suppressed sender — RFC 3551 /
/// ADR-0033 §1, invariant #1), a dynamic `payload_type`, and big-endian
/// `sequence` / `timestamp` / `ssrc`. Array-pattern destructuring keeps it free
/// of slice indexing.
#[must_use]
pub fn build_rtp_header(
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
) -> [u8; RTP_FIXED_HEADER_LEN] {
    let [s0, s1] = sequence.to_be_bytes();
    let [t0, t1, t2, t3] = timestamp.to_be_bytes();
    let [c0, c1, c2, c3] = ssrc.to_be_bytes();
    [
        // V=2 (top 2 bits); P=0, X=0, CC=0.
        RTP_VERSION << 6,
        // M=0 (continuous stream); 7-bit payload type.
        payload_type & 0x7f,
        s0,
        s1,
        t0,
        t1,
        t2,
        t3,
        c0,
        c1,
        c2,
        c3,
    ]
}

/// Encode interleaved unit-range `f32` samples to big-endian L16/L24 RTP payload
/// bytes (the whole-sample-group PCM body that rides directly in an RTP packet).
///
/// `samples` is interleaved frame-major; the caller (the sender) supplies a
/// whole number of sample groups, so no partial-group check is needed here.
#[must_use]
pub fn encode_pcm(samples: &[f32], depth: PcmDepth) -> Vec<u8> {
    // RED scaffold (ADR-0033 §1, replaced in the GREEN commit): the right byte
    // length but silence — the real big-endian L16/L24 encode lands next.
    vec![0u8; samples.len().saturating_mul(depth.bytes_per_sample())]
}
