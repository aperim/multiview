//! DEV-B4 audit: the lit display-sink bundle's teardown does a **blocking**
//! stop + thread-join (the audio sink polls `thread::is_finished` with a
//! `thread::sleep` up to its 500 ms detach bound; the video sink + hotplug
//! monitor block on a `JoinHandle::join`). Dropping that bundle directly inside
//! the async `Pipeline::drive_streaming` would run those blocking joins on the
//! Tokio worker thread executing the future — the Drop-join-in-async pattern the
//! engine safety rules forbid (no blocking in async).
//!
//! This proves the structural fix — `teardown_blocking_off_worker` moves the
//! bundle's blocking `Drop` onto `spawn_blocking` — keeps a concurrently-ready
//! future making progress *during* a wedged teardown. The real sink-handle
//! bundle needs hardware (KMS/ALSA); the generic `Send + 'static` seam proves
//! the worker stays free over **any** blocking-`Drop` bundle, which is exactly
//! the property the audit is about (the worker is not blocked).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use multiview_cli::pipeline::teardown_blocking_off_worker;

/// A stand-in for a display-sink handle whose `Drop` does a **blocking**
/// stop/join exactly like the real `DisplayAudioSink` / `DisplaySinkHandle` (a
/// `thread::sleep`-paced wait, then a `JoinHandle::join`). Here the block is a
/// fixed `thread::sleep` — the load-bearing property is that dropping it on the
/// async worker would freeze that worker for the sleep's duration.
struct WedgedHandle {
    block: Duration,
    dropped_at: Arc<Mutex<Option<Instant>>>,
}

impl Drop for WedgedHandle {
    fn drop(&mut self) {
        std::thread::sleep(self.block);
        *self.dropped_at.lock().unwrap() = Some(Instant::now());
    }
}

/// On a **truly single-threaded** (`current_thread`) runtime, a short witness
/// future scheduled *before* teardown must complete on its own short timeline
/// even while a 250 ms blocking teardown is in flight. With the teardown's
/// blocking `Drop` moved off the runtime thread (`spawn_blocking`) the runtime
/// thread is freed during the `.await`, so the witness runs at ~30 ms; with the
/// `Drop` run **on** the runtime thread the only thread is wedged and the
/// witness cannot run until it unblocks at ~250 ms. Asserting the witness
/// completed well inside the 250 ms block is the load-bearing, unambiguous
/// discriminator — a `current_thread` runtime cannot hide the block behind a
/// second worker, so this fails iff the teardown blocks the async thread.
#[tokio::test(flavor = "current_thread", start_paused = false)]
async fn teardown_does_not_block_the_async_worker() {
    let start = Instant::now();
    let block = Duration::from_millis(250);
    let witness_delay = Duration::from_millis(30);

    // The witness: after a short async sleep it records how long after `start`
    // it actually ran. On a free worker that is ~witness_delay; on a worker
    // wedged by the blocking Drop it cannot run until the block ends (~block).
    let witness_at = Arc::new(Mutex::new(None));
    let witness_at_bg = Arc::clone(&witness_at);
    let witness = tokio::spawn(async move {
        tokio::time::sleep(witness_delay).await;
        *witness_at_bg.lock().unwrap() = Some(start.elapsed());
    });

    // Begin the wedged teardown right away, so the witness's 30 ms timer comes
    // due *during* the 250 ms block. A free worker services it; a wedged one
    // cannot.
    let dropped_at = Arc::new(Mutex::new(None));
    let handle = WedgedHandle {
        block,
        dropped_at: Arc::clone(&dropped_at),
    };
    teardown_blocking_off_worker(handle, "test").await;
    let teardown_elapsed = start.elapsed();

    // Make sure the witness has finished recording (it has long since come due).
    witness.await.unwrap();
    let witness_elapsed = witness_at.lock().unwrap().expect("witness ran");

    // The bundle was actually torn down (its blocking `Drop` ran to completion):
    // the fix must not silently detach/leak the handles.
    assert!(
        dropped_at.lock().unwrap().is_some(),
        "teardown must still complete (the bundle's Drop ran)"
    );
    // Teardown really waited on the wedged `Drop` (not a no-op that skipped it).
    assert!(
        teardown_elapsed >= block,
        "teardown must actually wait for the blocking Drop (took {teardown_elapsed:?})"
    );
    // THE load-bearing assertion: the witness ran on its own ~30 ms timeline
    // *during* the 250 ms teardown — proving the worker stayed free. A worker
    // blocked by the Drop could not have run the witness until ~250 ms. The
    // ceiling (half the block) is comfortably above ~30 ms yet far below the
    // ~250 ms a blocked worker would force, robust on a loaded CI box.
    assert!(
        witness_elapsed < block / 2,
        "a concurrently-ready future must make progress during teardown; the \
         witness only ran after {witness_elapsed:?} (teardown blocked the worker \
         for the full {block:?})"
    );
}
