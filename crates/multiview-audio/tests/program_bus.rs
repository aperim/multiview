// ProgramBus (AUD-3): the per-output-tick program-bus mix driven by the
// SampleClock. Each tick pulls exactly `samples_per_tick` frames from every
// routed source's last-good store and mixes them, so the program audio rides the
// output clock with an exact, gap-free sample budget — the audio analogue of
// "exactly N frames for N ticks". A dropped/stalled source contributes silence
// (the store is silence-filling), never a gap, never an absent block.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use multiview_audio::format::{AudioBlock, AudioFormat, ChannelLayout};
use multiview_audio::program::ProgramBus;
use multiview_audio::store::AudioStore;
use multiview_core::time::Rational;

fn stereo48() -> AudioFormat {
    AudioFormat::new(48_000, ChannelLayout::Stereo)
}

#[test]
fn no_sources_emits_exact_gap_free_silence_budget() {
    // With no audio sources the program bus is still continuous: exactly
    // samples_per_tick frames of silence each tick, summing to the exact NTSC
    // long-run total (2000 ticks at 30000/1001 = 2000 * 1601.6 = 3_203_200).
    let fmt = stereo48();
    let mut bus = ProgramBus::new(fmt, Rational::new(30_000, 1001));
    let mut total = 0usize;
    for _ in 0..2000 {
        let b = bus.tick();
        assert_eq!(b.format(), fmt);
        assert!(
            b.frame_count() == 1601 || b.frame_count() == 1602,
            "each tick is the NTSC per-tick budget, got {}",
            b.frame_count()
        );
        assert!(
            b.interleaved().iter().all(|&s| s == 0.0),
            "no source => gap-free silence"
        );
        total += b.frame_count();
    }
    assert_eq!(total, 3_203_200);
}

#[test]
fn a_routed_source_is_mixed_into_the_program_bus() {
    let fmt = stereo48();
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    // Publish 4800 frames of a constant 0.5 (stereo -> 9600 interleaved samples).
    let block = AudioBlock::from_interleaved(fmt, vec![0.5f32; 4800 * 2]).unwrap();
    store.publish(&block).unwrap();

    let mut bus = ProgramBus::new(fmt, Rational::new(25, 1)); // 1920 samples/tick
    bus.add_source("in_a", Arc::clone(&store), 1.0);
    let b = bus.tick();
    assert_eq!(b.frame_count(), 1920);
    assert!(
        b.interleaved().iter().all(|&s| (s - 0.5).abs() < 1e-6),
        "the routed unity-gain 0.5 source must reach the program bus"
    );
}

#[test]
fn a_dropped_source_yields_silence_never_absent() {
    // A source whose decode stalled (an empty store, nothing ever published)
    // still yields an exact-budget block of silence every tick — never absent.
    let fmt = stereo48();
    let store = Arc::new(AudioStore::new(fmt, 48_000));
    let mut bus = ProgramBus::new(fmt, Rational::new(30, 1)); // 1600 samples/tick
    bus.add_source("dead", store, 1.0);
    for _ in 0..50 {
        let b = bus.tick();
        assert_eq!(b.frame_count(), 1600);
        assert!(b.interleaved().iter().all(|&s| s == 0.0));
    }
}
