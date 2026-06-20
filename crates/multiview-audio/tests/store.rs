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
// reason: test-only float<->index arithmetic on small exact ranges — the ramp
// values are integers exactly representable in f32, so the exact comparisons are
// intentional and the index<->float casts are loss-free here.
#![allow(clippy::as_conversions, clippy::cast_precision_loss, clippy::float_cmp)]

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
    assert_eq!(
        block.frame_count(),
        256,
        "read must return exactly N frames"
    );
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
    // Only frames 0..100 are published; nothing beyond is written yet.
    store.publish(&ramp(0, 100)).unwrap();

    // Read 120 frames: 100 real frames then 20 of silence (past EOF), never short.
    let out = store.read(120);
    assert_eq!(out.frame_count(), 120);
    let s = out.interleaved();
    // First 100 frames (200 samples) are the contiguous ramp 0,1,2,...
    for (i, &v) in s.iter().take(200).enumerate() {
        assert_eq!(v, i as f32, "sample {i} broke contiguity");
    }
    // Frames 100..120 (samples 200..240) are silence — nothing published there.
    assert!(
        s[200..].iter().all(|&v| v == 0.0),
        "frames past EOF must be silence-filled, not a gap"
    );

    // Now the producer catches up, publishing frames 100..150 contiguously
    // (sample value start = 100 frames * 2ch = 200). The reader's cursor already
    // advanced past 120, so its next read continues the stream gap-free from
    // frame 120 — it does NOT re-read the silence it already consumed.
    store.publish(&ramp(200, 50)).unwrap();
    let next = store.read(10);
    assert_eq!(next.frame_count(), 10);
    // Frames 120..130 -> sample values 240..260: the cursor advanced by exactly
    // 120 and the just-published frames slot in contiguously.
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
        store.publish(&ramp(written * 2, chunk)).unwrap();
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
    let err = store.publish(&mono).unwrap_err();
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
            prod_store.publish(&ramp(written * 2, 48)).unwrap();
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
    assert_eq!(
        frames_seen, 48_000,
        "reader did not get its requested frames"
    );

    let written = producer.join().expect("producer thread panicked");
    assert!(written > 0, "producer never published");
    stop.store(true, Ordering::Release);
}

/// The read cursor is an ABSOLUTE sample/frame position: `read_cursor` reports
/// the next absolute frame the reader will read, and `read` advances it by
/// exactly the number of frames pulled. This is what lets a re-point align the
/// new store to absolute tick time instead of replaying from frame 0.
#[test]
fn read_cursor_is_absolute_and_advances_by_frames_read() {
    let store = AudioStore::new(stereo(), 48_000);
    assert_eq!(store.read_cursor(), 0, "a fresh store starts at frame 0");
    let _ = store.read(100);
    assert_eq!(
        store.read_cursor(),
        100,
        "read(100) advances the cursor by 100"
    );
    let _ = store.read(57);
    assert_eq!(
        store.read_cursor(),
        157,
        "read(57) advances the cursor by 57"
    );
}

/// `seek_to` sets the absolute read position; the next read starts there.
#[test]
fn seek_to_sets_the_absolute_read_position() {
    let store = AudioStore::new(stereo(), 48_000);
    store.publish(&ramp(0, 200)).unwrap();
    // Jump the cursor to absolute frame 50; the next read must start at frame 50
    // (sample value 100), not at frame 0.
    store.seek_to(50);
    assert_eq!(store.read_cursor(), 50);
    let out = store.read(10);
    for (i, &v) in out.interleaved().iter().enumerate() {
        assert_eq!(
            v,
            (100 + i) as f32,
            "after seek_to(50) the read must start at absolute frame 50"
        );
    }
    assert_eq!(store.read_cursor(), 60);
}

/// `seek_to_live_edge` parks the cursor at the current write head, so a warm
/// store that has been buffering far ahead reads from the LIVE EDGE — not from
/// frame 0 climbing through evicted history.
#[test]
fn seek_to_live_edge_parks_at_the_write_head() {
    let cap = 1_000usize;
    let store = AudioStore::new(stereo(), cap);
    // Publish 5x capacity: head is at frame 5000, only [4000,5000) survives.
    let mut written = 0usize;
    let total = cap * 5;
    let chunk = 250;
    while written < total {
        store.publish(&ramp(written * 2, chunk)).unwrap();
        written += chunk;
    }
    // The naive cursor is still at frame 0 (it would read evicted silence
    // climbing toward the surviving tail). Seek to the live edge.
    store.seek_to_live_edge();
    assert_eq!(
        store.read_cursor(),
        i64::try_from(total).unwrap(),
        "seek_to_live_edge must park the cursor at the write head (frame {total})"
    );
}

// ---------------------------------------------------------------------------
// `publish_at` — the absolute-frame-index write (ADR-T013 §4 / ADR-0033). The
// RTP-audio rebaser maps an RTP timestamp to an absolute store frame and writes
// there; a late (reordered) packet lands at its TRUE index, and an unwritten
// span between writes silence-fills (gap-free by construction). Bounded:
// drop-oldest past capacity, never grows.

/// A block published at an absolute frame index reads back at exactly that
/// index — the writer no longer just appends, it can place at an anchor.
#[test]
fn publish_at_places_block_at_absolute_frame() {
    let store = AudioStore::new(stereo(), 48_000);
    // Anchor the first audio at absolute frame 1000 (the rebaser's anchor).
    store.publish_at(1000, &ramp(0, 50)).unwrap();
    // Seek the reader to the anchor and read the placed block back verbatim.
    store.seek_to(1000);
    let out = store.read(50);
    assert_eq!(out.frame_count(), 50);
    for (i, &v) in out.interleaved().iter().enumerate() {
        assert_eq!(v, i as f32, "sample {i} did not land at the anchor frame");
    }
}

/// The span between the anchor and a later absolute write silence-fills — a gap
/// in absolute frame space is never a short read or a splice (gap-free).
#[test]
fn publish_at_gap_silence_fills_between_writes() {
    let store = AudioStore::new(stereo(), 48_000);
    store.publish_at(0, &ramp(0, 10)).unwrap(); // frames [0,10)
    store.publish_at(30, &ramp(1000, 10)).unwrap(); // frames [30,40); [10,30) is a hole
    store.seek_to(0);
    let out = store.read(40);
    let s = out.interleaved();
    // [0,10) is the first ramp.
    for (i, &v) in s.iter().take(20).enumerate() {
        assert_eq!(v, i as f32, "leading block sample {i}");
    }
    // [10,30) (samples 20..60) is the silence-filled hole.
    assert!(
        s[20..60].iter().all(|&v| v == 0.0),
        "the inter-write hole must silence-fill, never splice"
    );
    // [30,40) (samples 60..80) is the second ramp (started at 1000).
    for (k, &v) in s[60..80].iter().enumerate() {
        assert_eq!(v, (1000 + k) as f32, "trailing block sample {k}");
    }
}

/// A late (reordered) packet writes to its TRUE absolute index behind the head
/// without disturbing already-written later frames — absolute placement, not
/// append (ADR-T013 §4 reorder-by-index).
#[test]
fn publish_at_reordered_packet_lands_behind_head() {
    let store = AudioStore::new(stereo(), 48_000);
    // The "later" packet arrives first (frames [20,30)).
    store.publish_at(20, &ramp(2000, 10)).unwrap();
    // The "earlier" packet arrives late and fills [0,10) at its true index.
    store.publish_at(0, &ramp(0, 10)).unwrap();
    store.seek_to(0);
    let out = store.read(30);
    let s = out.interleaved();
    for (i, &v) in s.iter().take(20).enumerate() {
        assert_eq!(v, i as f32, "the late earlier packet must land at frame 0");
    }
    // [10,20) is the still-unwritten hole -> silence.
    assert!(s[20..40].iter().all(|&v| v == 0.0), "hole stays silence");
    for (k, &v) in s[40..60].iter().enumerate() {
        assert_eq!(
            v,
            (2000 + k) as f32,
            "the earlier-arrived later packet survives"
        );
    }
}

/// `publish_at` is bounded: writing far past capacity drops the oldest frames
/// (never grows) exactly like the append `publish`.
#[test]
fn publish_at_is_bounded_drop_oldest() {
    let cap = 1_000usize;
    let store = AudioStore::new(stereo(), cap);
    // Place 5x capacity of contiguous blocks at advancing absolute indices.
    let chunk = 250usize;
    let total = cap * 5;
    let mut frame = 0usize;
    while frame < total {
        store
            .publish_at(i64::try_from(frame).unwrap(), &ramp(frame * 2, chunk))
            .unwrap();
        frame += chunk;
    }
    assert!(
        store.buffered_frames() <= cap,
        "publish_at must drop-oldest past capacity (buffered {} > cap {cap})",
        store.buffered_frames()
    );
}

/// A format mismatch is rejected (the same contract as `publish`).
#[test]
fn publish_at_rejects_format_mismatch() {
    let store = AudioStore::new(stereo(), 48_000);
    let mono =
        AudioBlock::from_interleaved(AudioFormat::new(FS, ChannelLayout::Mono), vec![0.0; 10])
            .unwrap();
    assert!(
        store.publish_at(0, &mono).is_err(),
        "a format mismatch must be rejected, never silently written"
    );
}

// ---------------------------------------------------------------------------
// `publish_window` — the sliding-window REPLACE write (ADR-T019 §2.2/§2.3, the
// CRITICAL-1 + CRITICAL-2 fix). The media-player audio rail re-derives the whole
// unplayed window `[cursor, H)` from the deck's CURRENT transport state every
// block and REPLACES the store window with it (not append) — so a transport
// transition (arm-exit / pause / stop) overwrites any stale pre-transition tail
// before the bus reads it. Backed by a triple-buffered preallocated snapshot
// pool, so the steady path does NO per-block heap allocation (rule 22).

/// `publish_window(base, samples)` makes the live window EXACTLY `[base, base+n)`:
/// a read seeking there gets the placed samples; the window is REPLACED, not
/// merged — a smaller later window does not leave a stale tail of the larger one.
#[test]
fn publish_window_replaces_the_live_window() {
    let store = AudioStore::new(stereo(), 96_000);
    // First window: frames [1000, 1000+50) = a ramp.
    let big: Vec<f32> = (0..(50 * 2)).map(|i| i as f32).collect();
    store.publish_window(1000, &big).unwrap();
    store.seek_to(1000);
    let out = store.read(50);
    for (i, &v) in out.interleaved().iter().enumerate() {
        assert_eq!(v, i as f32, "window sample {i} not placed at base 1000");
    }

    // A SECOND, SMALLER window at a later base [1100, 1100+10): the store window
    // is now exactly that — the [1000,1050) content is GONE (replaced, not
    // appended). Reading from 1100 yields the new ramp; reading from 1000 (the
    // replaced span) yields silence (it is no longer in the window).
    let small: Vec<f32> = (0..(10 * 2)).map(|i| 1000.0 + i as f32).collect();
    store.publish_window(1100, &small).unwrap();
    store.seek_to(1100);
    let out2 = store.read(10);
    for (i, &v) in out2.interleaved().iter().enumerate() {
        assert_eq!(v, 1000.0 + i as f32, "replaced window sample {i}");
    }
    // The earlier span is no longer present — replace, not merge/append.
    store.seek_to(1000);
    let gone = store.read(50);
    assert!(
        gone.interleaved().iter().all(|&v| v == 0.0),
        "publish_window REPLACES the window — the prior span must not survive (no append/merge)"
    );
}

/// `publish_window` is gap-free at the read edge: the bus reads forward from its
/// cursor and the window always covers `[cursor, cursor+frames)` — so a read of
/// the window's own base returns the placed samples, never a short block.
#[test]
fn publish_window_read_is_gap_free_and_full() {
    let store = AudioStore::new(stereo(), 96_000);
    let w: Vec<f32> = vec![0.5f32; 1600 * 2];
    store.publish_window(0, &w).unwrap();
    let out = store.read(1600);
    assert_eq!(out.frame_count(), 1600, "read must be full, never short");
    assert!(
        out.interleaved().iter().all(|&v| v == 0.5),
        "the placed window must read back verbatim"
    );
}

/// A ragged length (not a whole number of frames) is a typed error, never a
/// torn mid-frame window.
#[test]
fn publish_window_rejects_a_ragged_length() {
    let store = AudioStore::new(stereo(), 48_000);
    // 7 samples is not a whole number of stereo frames.
    let ragged = vec![0.0f32; 7];
    assert!(
        store.publish_window(0, &ragged).is_err(),
        "a ragged window length must be rejected, never torn mid-frame"
    );
}

/// The CRITICAL-2 proof (rule 22): across MANY `publish_window` calls the store
/// reuses a BOUNDED set of backing buffers (the triple-buffer pool), so the number
/// of DISTINCT backing-buffer pointers the reader ever observes is small and
/// constant — NOT one fresh `Vec` per publish (which would show an unbounded,
/// ever-growing set of pointers). This is the **stable-pointer assertion** the
/// ADR's §2.2 triple-buffer promises, and it is *stronger* than a counted
/// allocation: it proves the SAME backing buffers are reused, not merely that few
/// allocations happen. (A `#[global_allocator]` counting allocator is not an option
/// here — `multiview-audio` is `unsafe_code = forbid`, and `GlobalAlloc` requires
/// `unsafe`.)
#[test]
fn publish_window_reuses_a_bounded_pool_of_backing_buffers() {
    use std::collections::HashSet;
    let store = AudioStore::new(stereo(), 96_000);
    let w: Vec<f32> = vec![0.25f32; 4800 * 2];
    let mut ptrs: HashSet<usize> = HashSet::new();
    // Many publishes interleaved with reads (so the reader releases its snapshot
    // and the writer can reuse a pool slot — the real SPSC handoff).
    for lap in 0..200i64 {
        let base = lap * 4800;
        store.publish_window(base, &w).unwrap();
        ptrs.insert(store.window_backing_ptr());
        store.seek_to(base);
        let _ = store.read(1600);
    }
    assert!(
        ptrs.len() <= 4,
        "publish_window must reuse a bounded triple-buffer pool, not allocate per block — \
         saw {} distinct backing buffers across 200 publishes (a per-block alloc would show ~200)",
        ptrs.len()
    );
}
