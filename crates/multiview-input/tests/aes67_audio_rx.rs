//! ST 2110-30 / AES67 PCM-audio receive tests (ADR-0033 §3, ADR-T013).
//!
//! The pure `pcm_to_f32` decode (always compiled, default build) and — behind
//! the `st2110` feature — the `Aes67AudioProducer` that pulls RTP packet units,
//! depacketizes each into interleaved `f32`, and yields an [`Aes67AudioFrame`]
//! carrying the verbatim RTP timestamp + SSRC for the ADR-T013 rebase seam.
//!
//! The decode round-trips against the sibling `Aes67Packetizer` egress codec:
//! `f32 -> L16/L24 bytes -> V30Payload -> f32` reproduces the input within the
//! wire quantization.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::float_cmp
)]

use multiview_input::st2110::v30::{Aes3Format, SampleDepth, V30Payload};
use multiview_input::st2110::{pcm_to_f32, Aes67Packetizer};

/// The largest L24 round-trip error: encode scales by `2^23 - 1`, decode by
/// `2^23`, so a full-scale sample lands ~`1/2^23` short — well under this bound.
const ROUNDTRIP_EPS: f32 = 1.0e-4;

#[test]
fn pcm_to_f32_roundtrips_l24_through_the_packetizer() {
    let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
    let packetizer = Aes67Packetizer::new(2, SampleDepth::L24).expect("stereo L24");

    // Two stereo sample groups, including a full-scale-negative sample.
    let original = vec![0.5_f32, -0.25, 0.75, -1.0];
    let bytes = packetizer.encode(&original).expect("encode whole groups");
    let payload = V30Payload::parse(&bytes, format).expect("whole sample groups");

    let decoded = pcm_to_f32(&payload);
    assert_eq!(decoded.len(), original.len(), "one f32 per encoded sample");
    for (want, got) in original.iter().zip(&decoded) {
        assert!(
            (want - got).abs() < ROUNDTRIP_EPS,
            "L24 round-trip within quantization: want {want}, got {got}",
        );
    }
}

#[test]
fn pcm_to_f32_roundtrips_l16_through_the_packetizer() {
    let format = Aes3Format::new(1, SampleDepth::L16).expect("mono L16");
    let packetizer = Aes67Packetizer::new(1, SampleDepth::L16).expect("mono L16");

    let original = vec![0.0_f32, 0.5, -0.5, 0.999];
    let bytes = packetizer.encode(&original).expect("encode whole groups");
    let payload = V30Payload::parse(&bytes, format).expect("whole sample groups");

    let decoded = pcm_to_f32(&payload);
    assert_eq!(decoded.len(), original.len());
    for (want, got) in original.iter().zip(&decoded) {
        assert!(
            (want - got).abs() < 1.0e-3,
            "L16 round-trip within quantization: want {want}, got {got}",
        );
    }
}

#[cfg(feature = "st2110")]
mod producer {
    use super::{Aes3Format, Aes67Packetizer, SampleDepth, ROUNDTRIP_EPS};
    use multiview_input::st2110::transport::{PacketSource, St2110Packet};
    use multiview_input::st2110::Aes67AudioProducer;

    /// A scripted [`PacketSource`] that yields injected units front-to-back,
    /// then `Ok(None)` (mirrors the `ScriptedSource` in `st2110_ingest.rs`).
    struct ScriptedSource {
        packets: std::collections::VecDeque<St2110Packet>,
    }

    impl ScriptedSource {
        fn new(packets: Vec<St2110Packet>) -> Self {
            Self {
                packets: packets.into(),
            }
        }
    }

    impl PacketSource for ScriptedSource {
        fn poll_packet(&mut self) -> Result<Option<St2110Packet>, multiview_input::Error> {
            Ok(self.packets.pop_front())
        }
    }

    /// Wrap an encoded AES67 payload in a post-RTP-parse packet unit.
    fn audio_packet(timestamp: u32, sequence: u16, ssrc: u32, payload: Vec<u8>) -> St2110Packet {
        St2110Packet {
            marker: false,
            timestamp,
            sequence,
            ssrc,
            payload,
        }
    }

    #[test]
    fn producer_yields_depacketized_audio_with_raw_timestamp_and_ssrc() {
        let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
        let packetizer = Aes67Packetizer::new(2, SampleDepth::L24).expect("stereo L24");
        let samples = vec![0.5_f32, -0.5, 0.25, -0.25];
        let payload = packetizer.encode(&samples).expect("encode whole groups");

        let source = ScriptedSource::new(vec![audio_packet(1_000, 7, 0xDEAD_BEEF, payload)]);
        let mut producer = Aes67AudioProducer::new(Box::new(source), format);

        let frame = producer
            .next_audio()
            .expect("poll never faults")
            .expect("one audio unit is yielded");
        assert_eq!(frame.raw_timestamp, 1_000, "verbatim RTP media timestamp");
        assert_eq!(frame.ssrc, 0xDEAD_BEEF, "SSRC forwarded for the rebaser");
        assert!(!frame.discontinuity, "the first packet is not a gap");
        assert_eq!(frame.samples.len(), samples.len());
        for (want, got) in samples.iter().zip(&frame.samples) {
            assert!(
                (want - got).abs() < ROUNDTRIP_EPS,
                "decoded sample within quantization: want {want}, got {got}",
            );
        }

        assert!(
            producer.next_audio().expect("poll never faults").is_none(),
            "the source drains to None (sampled, never blocks — inv #1)"
        );
    }

    #[test]
    fn producer_skips_malformed_payload_without_faulting() {
        let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
        let packetizer = Aes67Packetizer::new(2, SampleDepth::L24).expect("stereo L24");
        let good = packetizer
            .encode(&[0.1_f32, 0.2])
            .expect("one stereo group");

        // A 5-byte payload is not a whole number of 6-byte stereo-L24 groups.
        let source = ScriptedSource::new(vec![
            audio_packet(10, 1, 1, vec![0, 1, 2, 3, 4]),
            audio_packet(20, 2, 1, good),
        ]);
        let mut producer = Aes67AudioProducer::new(Box::new(source), format);

        // The malformed unit is skipped; the next good unit is yielded — the
        // producer never faults on a single bad datagram (inv #1 / #2).
        let frame = producer
            .next_audio()
            .expect("a bad payload is skipped, not faulted")
            .expect("the following good unit is yielded");
        assert_eq!(frame.raw_timestamp, 20, "skipped straight to the good unit");
    }

    #[test]
    fn producer_flags_a_sequence_gap_as_discontinuity() {
        let format = Aes3Format::new(1, SampleDepth::L16).expect("mono L16");
        let packetizer = Aes67Packetizer::new(1, SampleDepth::L16).expect("mono L16");
        let payload = packetizer.encode(&[0.0_f32]).expect("one mono group");

        // Sequence 1 then 3 — packet 2 was lost.
        let source = ScriptedSource::new(vec![
            audio_packet(0, 1, 1, payload.clone()),
            audio_packet(48, 3, 1, payload),
        ]);
        let mut producer = Aes67AudioProducer::new(Box::new(source), format);

        let first = producer.next_audio().unwrap().expect("first unit");
        assert!(!first.discontinuity, "the first unit anchors, no gap");
        let second = producer.next_audio().unwrap().expect("second unit");
        assert!(
            second.discontinuity,
            "a sequence gap (1 -> 3) is flagged as a discontinuity"
        );
    }

    #[test]
    fn ssrc_change_resets_the_sequence_watermark() {
        // P2-F5: the sequence watermark must be SSRC-scoped. A new SSRC is a new
        // stream with its OWN sequence space; if the watermark is still judged
        // against the OLD SSRC's value, a new-stream sequence that serial
        // arithmetic reads as "before" the old watermark is treated as stale and
        // never updates it — so real gaps on the new stream are silently missed.
        let format = Aes3Format::new(1, SampleDepth::L16).expect("mono L16");
        let packetizer = Aes67Packetizer::new(1, SampleDepth::L16).expect("mono L16");
        let pcm = packetizer.encode(&[0.0_f32]).expect("one mono group");

        // Stream A at a low sequence, then stream B (a NEW SSRC) at a HIGH
        // sequence base that RFC 1982 arithmetic reads as "before" A's, then a
        // genuine gap on B (60001 -> 60003, packet 60002 lost).
        let source = ScriptedSource::new(vec![
            audio_packet(0, 100, 0xAAAA_AAAA, pcm.clone()), // A anchors
            audio_packet(0, 60_000, 0xBBBB_BBBB, pcm.clone()), // B: new stream anchors
            audio_packet(48, 60_001, 0xBBBB_BBBB, pcm.clone()), // B: contiguous
            audio_packet(96, 60_003, 0xBBBB_BBBB, pcm),     // B: 60001 -> 60003 gap
        ]);
        let mut producer = Aes67AudioProducer::new(Box::new(source), format);

        let a = producer.next_audio().unwrap().expect("A anchors");
        assert!(!a.discontinuity, "first packet of stream A anchors, no gap");
        let b0 = producer.next_audio().unwrap().expect("B anchor");
        assert!(
            !b0.discontinuity,
            "the first packet of a NEW SSRC anchors the new stream, not a gap"
        );
        let b1 = producer.next_audio().unwrap().expect("B contiguous");
        assert!(
            !b1.discontinuity,
            "60000 -> 60001 is contiguous on stream B"
        );
        let b2 = producer.next_audio().unwrap().expect("B gap");
        assert!(
            b2.discontinuity,
            "a real gap on the new stream (60001 -> 60003) must be flagged — the \
             watermark tracks THIS SSRC, not the stale prior one (P2-F5)"
        );
    }

    /// A [`PacketSource`] that yields `remaining` malformed units — each a
    /// counted poll — then drains to `None`, so a test can prove the producer
    /// never drains an unbounded flood in one call (F1 / inv #1).
    struct FloodSource {
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        remaining: usize,
    }

    impl PacketSource for FloodSource {
        fn poll_packet(&mut self) -> Result<Option<St2110Packet>, multiview_input::Error> {
            self.polls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.remaining == 0 {
                return Ok(None);
            }
            self.remaining -= 1;
            // A 5-byte payload is never a whole number of 6-byte stereo-L24
            // groups, so `V30Payload::parse` fails and the producer skips it.
            Ok(Some(audio_packet(0, 0, 1, vec![0, 1, 2, 3, 4])))
        }
    }

    #[test]
    fn next_audio_is_bounded_work_under_a_malformed_flood() {
        use std::sync::atomic::Ordering;

        // Any sane per-poll budget is far below this ceiling; a malformed flood
        // that drains unbounded work in one call blows past it (F1 / inv #1).
        const POLL_BUDGET_CEILING: usize = 1_000;
        const FLOOD: usize = 5_000;

        let format = Aes3Format::new(2, SampleDepth::L24).expect("stereo L24");
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source = FloodSource {
            polls: std::sync::Arc::clone(&polls),
            remaining: FLOOD,
        };
        let mut producer = Aes67AudioProducer::new(Box::new(source), format);

        // One tick over a malformed flood: no valid unit is produced, and the
        // call must NOT drain the whole flood — a sample is bounded work so it
        // can never delay the output clock (inv #1). The caller re-polls next
        // tick; malformed datagrams are drained at the budget rate, not at once.
        let out = producer
            .next_audio()
            .expect("a malformed flood is skipped, never faulted");
        assert!(out.is_none(), "no valid unit in the flood this tick");
        let polled = polls.load(Ordering::Relaxed);
        assert!(
            polled <= POLL_BUDGET_CEILING,
            "next_audio must be bounded per call (inv #1): polled {polled} of a \
             {FLOOD}-packet flood, exceeding the {POLL_BUDGET_CEILING} ceiling",
        );
    }
}
