// Bounded last-good audio store tests (pure-Rust; no `ffmpeg` feature needed —
// the store operates on in-memory `AudioBlock`s). These prove the engine-side
// `read` is gap-free, contiguous, bounded (drop-oldest), and never blocks, the
// audio analogue of the video tile store's last-good-frame guarantee
// (ADR-R005 §4.1, resilience-and-av §4.1 — silence-fill keeps tracks gap-free,
// load-bearing for invariant A).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// reason: test-only float<->index arithmetic on small exact ranges.
#![allow(clippy::as_conversions, clippy::cast_precision_loss)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use multiview_audio::store::AudioStore;
use multiview_audio::{AudioBlock, AudioFormat, ChannelLayout};

const FS: u32 = 48_000;

fn stereo() -> AudioFormat {
    AudioFormat::new(FS, ChannelLayout::Stereo)
}

/// A ramp block of `frames` stereo frames whose first interleaved sample is
/// `start` and increments by 1.0 each *sample* (so a contiguous read across
/// blocks is a continuous ramp — any gap or reorder shows up immediately).
fn ramp(start: usize, frames: usize) -> AudioBlock {
    let channels = 2;
    let mut s = Vec::with_capacity(frames * channels);
    for i in 0..(frames * channels) {
        s.push((start + i) as f32);
    }
    AudioBlock::from_interleaved(stereo(), s).unwrap()
}

/// A freshly-created store yields exactly the requested number of *silence*
/// frames (never blocks, never gaps) before anything is published.
#[test]
fn empty_store_reads_silence_not_a_gap() {
    let store = AudioStore::new(stereo(), 48_000);
    let block = store.read(256);
    assert_eq!(block.frame_count(), 256, "read must return exactly N frames");
    assert_eq!(block.format(), stereo());
    assert!(
        block.interleaved().iter().all(|&s| s == 0.0),
        "an empty store must read silence, never a short/absent block"
    );
}

/// Published samples read back contiguously and in order across block
/// boundaries, then run into silence past what was published — no gap, no
/// duplication, no reorder.
#[test]
fn reads_are_contiguous_then_silence_past_eof() {
    let store = AudioStore::new(stereo(), 48_000);
    // Two ramps published back to back: frames 0..100 then 100..150.
    store.publish(ramp(0, 100)).unwrap();
    store.publish(ramp(200, 50)).unwrap(); // sample value start=200 (== 100 frames*2ch)

    // Read 120 frames: 100 real frames then 20 of silence.
    let out = store.read(120);
    assert_eq!(out.frame_count(), 120);
    let s = out.interleaved();
    // First 100 frames (200 samples) are the contiguous ramp 0,1,2,...
    for (i, &v) in s.iter().take(200).enumerate() {
        assert_eq!(v, i as f32, "sample {i} broke contiguity");
    }
    // Next 20 frames are silence (nothing published there yet).
    assert!(
        s[200..].iter().all(|&v| v == 0.0),
        "frames past EOF must be silence-filled"
    );

    // The cursor advanced exactly 120 frames: the next read starts at frame 120,
    // i.e. interleaved sample value 240 (frame 120 * 2ch), continuing the second
    // ramp (which covers frames 100..150 with values 200..300).
    let next = store.read(10);
    assert_eq!(next.frame_count(), 10);
    // Frames 120..130 -> sample values 240..260.
    for (i, &v) in next.interleaved().iter().enumerate() {
        assert_eq!(v, (240 + i) as f32, "read cursor did not advance by 120");
    }
}

/// The ring is bounded: publishing far more than capacity drops the OLDEST
/// samples (never grows). A reader that falls behind past capacity reads
/// silence for the evicted region, then catches the surviving tail — it never
/// blocks the writer and the store never grows unbounded.
#[test]
fn bounded_ring_drops_oldest_never_grows() {
    let cap = 1_000usize;
    let store = AudioStore::new(stereo(), cap);
    // Publish 5x capacity without ever reading.
    let total = cap * 5;
    let chunk = 250;
    let mut written = 0;
    while written < total {
        store.publish(ramp(written * 2, chunk)).unwrap();
        written += chunk;
    }
    assert!(
        store.buffered_frames() <= cap,
        "ring exceeded its bound: {} > {cap}",
        store.buffered_frames()
    );

    // The reader is at frame 0, but only the last `cap` frames survive. Reading
    // the whole written span: the evicted head is silence, the surviving tail is
    // the real ramp. Crucially: still exactly `total` frames, never short.
    let out = store.read(total);
    assert_eq!(out.frame_count(), total);
    // The last `cap` frames must be the surviving tail of the ramp, contiguous.
    let s = out.interleaved();
    let tail_start_frame = total - cap;
    for f in tail_start_frame..total {
        let lv = s[f * 2];
        let rv = s[f * 2 + 1];
        assert_eq!(lv, (f * 2) as f32, "surviving tail frame {f} L corrupt");
        assert_eq!(rv, (f * 2 + 1) as f32, "surviving tail frame {f} R corrupt");
    }
}

/// Format mismatch is a typed error, never a panic or silent corruption.
#[test]
fn publish_format_mismatch_is_an_error() {
    let store = AudioStore::new(stereo(), 48_000);
    let mono = AudioBlock::silence(AudioFormat::new(FS, ChannelLayout::Mono), 10);
    let err = store.publish(mono).unwrap_err();
    assert!(
        matches!(err, multiview_audio::AudioError::FormatMismatch { .. }),
        "publishing a mismatched-format block must be a FormatMismatch error, got {err:?}"
    );
}

/// The store is the SPSC handoff between a decode/producer thread and the
/// engine reader: a producer thread drives blocks in while the reader pulls
/// fixed-size chunks. The reader NEVER blocks waiting for the producer and the
/// total frames it observes equal the chunks it asked for (gap-free), with the
/// real samples appearing as a contiguous prefix. This is the invariant-#10
/// "engine only samples, never back-pressures" property under real concurrency.
#[test]
fn producer_thread_never_back_pressures_reader() {
    let store = Arc::new(AudioStore::new(stereo(), 96_000));
    let stop = Arc::new(AtomicBool::new(false));

    // Producer: publish 1000 blocks of 48 frames (48_000 frames total).
    let prod_store = Arc::clone(&store);
    let prod_stop = Arc::clone(&stop);
    let producer = std::thread::spawn(move || {
        let mut written = 0usize;
        for _ in 0..1000 {
            if prod_stop.load(Ordering::Acquire) {
                break;
            }
            prod_store.publish(ramp(written * 2, 48)).unwrap();
            written += 48;
        }
        written
    });

    // Reader: pull 500 reads of 96 frames (== 48_000 frames) WITHOUT ever
    // blocking on the producer. Every read returns exactly 96 frames.
    let mut frames_seen = 0usize;
    for _ in 0..500 {
        let out = store.read(96);
        assert_eq!(out.frame_count(), 96, "read returned a short/gapped block");
        frames_seen += out.frame_count();
    }
    assert_eq!(frames_seen, 48_000, "reader did not get its requested frames");

    let written = producer.join().expect("producer thread panicked");
    assert!(written > 0, "producer never published");
    stop.store(true, Ordering::Release);
}
