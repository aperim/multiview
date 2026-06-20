//! CRITICAL-2 (rule 22) counting-allocator proof for `AudioStore::publish_window`
//! (ADR-T019 §2.2): the media-player audio rail re-derives and REPLACES the whole
//! unplayed window every block, so this path runs once per refill on the data
//! plane — it MUST NOT allocate per block. The round-2 `publish_samples` allocated
//! a fresh `Vec` + `Arc` on every publish (the COW append); `publish_window` over
//! a triple-buffered preallocated snapshot pool allocates only during warm-up.
//!
//! A process-global counting allocator (isolated to this test binary) counts heap
//! allocations during a steady publish loop and asserts the count is ZERO after
//! the pool is warm — the unambiguous "no per-block heap allocation" gate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::as_conversions, clippy::cast_precision_loss)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use multiview_audio::store::AudioStore;
use multiview_audio::{AudioFormat, ChannelLayout};

/// A pass-through allocator that counts allocations while `COUNTING` is enabled.
struct CountingAlloc;

static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static COUNTING: AtomicBool = AtomicBool::new(false);

// SAFETY: delegates every operation verbatim to the system allocator; the only
// added behaviour is a relaxed atomic counter increment on allocation while the
// `COUNTING` flag is set. No other invariant is touched.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn stereo() -> AudioFormat {
    AudioFormat::new(AudioFormat::CANONICAL_RATE, ChannelLayout::Stereo)
}

/// After the triple-buffer pool is warm, a steady `publish_window` + `read` loop
/// performs ZERO heap allocations (rule 22). A per-block `Vec`/`Arc` allocation
/// (the round-2 `publish_samples` shape) would make this count climb with the
/// loop iterations.
#[test]
fn publish_window_does_not_allocate_per_block() {
    let store = AudioStore::new(stereo(), 96_000);
    let window: Vec<f32> = vec![0.3f32; 4800 * 2]; // 0.1 s stereo window

    // Warm-up: prime the triple-buffer pool (the first few publishes allocate the
    // pool's snapshot buffers; that is one-time, not per-block). Interleave reads
    // so the reader cycles the pool exactly as the real SPSC handoff does.
    for lap in 0..8i64 {
        let base = lap * 4800;
        store.publish_window(base, &window).unwrap();
        store.seek_to(base);
        let _ = store.read(1600);
    }

    // Now count allocations across a long steady loop: the pool is warm, so the
    // publish path must reuse its buffers and allocate nothing. (`read` returns an
    // `AudioBlock` that DOES allocate its own output `Vec`, so we do NOT read
    // inside the counted region — the gate is specifically that the STORE WRITE
    // does not allocate per block. The reader's snapshot is released by the prior
    // warm-up reads, freeing pool slots for reuse.)
    ALLOC_COUNT.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    for lap in 8..208i64 {
        store.publish_window(lap * 4800, &window).unwrap();
    }
    COUNTING.store(false, Ordering::Relaxed);

    let allocs = ALLOC_COUNT.load(Ordering::Relaxed);
    assert_eq!(
        allocs, 0,
        "publish_window allocated {allocs} times across 200 steady publishes — \
         it must reuse the triple-buffer pool (rule 22 / ADR-T019 §2.2), not allocate per block"
    );
}
