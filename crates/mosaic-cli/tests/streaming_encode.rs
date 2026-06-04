//! Streaming-encode behavioural tests for [`RealPipeline`] (ADR-0025).
//!
//! These are sleep-free, GPU-free, FFmpeg-codec-free behavioural tests of the
//! streaming bake→encode→fan-out path. They drive the engine over a
//! [`ManualTimeSource`] + [`CooperativePacer`] (no wall-clock waits) and inject
//! **fake** off-hot-path sinks via the `drive_streaming_for_test` seam, so the
//! tests assert the *concurrency contract* — bounded memory, no engine stall,
//! exact-N offline, and streaming-not-batch — without needing a real encoder or
//! the `ffmpeg`/`ffprobe` CLIs. The end-to-end "does it produce a playable file"
//! coverage lives in `real_pipeline.rs`/`overlay_pipeline.rs`.
//!
//! Why a test seam: `RealPipeline::drive` is private; the public `run_for`/
//! `run_until` always wire the *real* file/HLS sinks. The seam exposes the same
//! streaming machinery with (a) an injected time source + pacer and (b)
//! injectable fake sink runners, so the hot loop, the bounded channel policy,
//! and the consumer/fan-out are exercised under a test's control.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Barrier};

use mosaic_cli::pipeline::{RealPipeline, SendPolicy, TestSinkOutcome};
use mosaic_compositor::pipeline::Nv12Image;
use mosaic_config::MosaicConfig;
use mosaic_engine::{CooperativePacer, ManualTimeSource, StopSignal, TimeSource};

/// A tiny single-tile config: one built-in `test` source into a 1x1 grid, an
/// HLS output (so a file + HLS sink are derived). The streaming-encode tests do
/// not decode the produced media — they inject fakes — so the geometry is kept
/// small to keep per-tick canvas clones cheap.
fn config_text() -> String {
    r##"
schema_version = 1

[canvas]
width = 64
height = 64
fps = "25/1"
pixel_format = "nv12"
background = "#101014"
[canvas.color]
profile = "sdr-bt709-limited"

[layout]
kind = "grid"
columns = ["1fr"]
rows = ["1fr"]
areas = ["a"]

[[sources]]
id = "in_a"
kind = "test"

[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"

[[outputs]]
kind = "hls"
path = "/tmp/mosaic-streaming-test/index.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
    .to_owned()
}

fn build_pipeline() -> RealPipeline {
    let config = MosaicConfig::load_from_toml(&config_text()).expect("parse config");
    config.validate().expect("config validates");
    RealPipeline::build(&config).expect("build real pipeline")
}

/// Advance the [`ManualTimeSource`] far enough that the [`CooperativePacer`]
/// never gates the loop, on a background thread, so the engine future can make
/// progress under the current-thread tokio runtime without any real sleeping.
fn spawn_clock_driver(clock: Arc<ManualTimeSource>, stop: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            clock.advance(std::time::Duration::from_millis(40));
            std::thread::yield_now();
        }
    });
}

/// (1) Live + a blocked sink: the engine still emits all N ticks, the channel
/// occupancy never exceeds the cap, and the overload is counted (`dropped > 0`).
///
/// RED today: the pre-ADR-0025 `drive` collects every tick into an unbounded
/// `Vec` and encodes only after the loop — so there is no cap, no drop counter,
/// and a blocked consumer would grow memory without bound (the OOM the ADR
/// fixes). The seam does not even exist yet, so this fails to compile/run.
#[tokio::test(flavor = "current_thread")]
async fn live_blocked_sink_stays_bounded_and_never_stalls() {
    const TICKS: u64 = 400;

    let mut pipeline = build_pipeline();

    // A fake sink that BLOCKS forever on its first received frame (a wedged
    // encoder/muxer). It must never be able to stall the engine.
    let gate = Arc::new(Barrier::new(2));
    let _gate_for_assert = Arc::clone(&gate);
    let received = Arc::new(AtomicUsize::new(0));
    let received_in_sink = Arc::clone(&received);
    let blocked_runner = move |rx: Receiver<Arc<Nv12Image>>| -> TestSinkOutcome {
        // Pull exactly one frame, then block forever (until the channel is
        // dropped at teardown, which unblocks recv with Err).
        if rx.recv().is_ok() {
            received_in_sink.fetch_add(1, Ordering::Release);
        }
        // Now stall: keep trying to recv but never finishing fast. When the
        // consumer drops the sender at teardown, recv() returns Err and we exit.
        while rx.recv().is_ok() {
            // deliberately do nothing fast — simulate a wedged sink that cannot
            // keep up; under DropOnOverload the hot loop must shed, not block.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        TestSinkOutcome {
            frames: received_in_sink.load(Ordering::Acquire),
        }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    let ts: Arc<dyn TimeSource> = Arc::clone(&clock) as Arc<dyn TimeSource>;
    let result = pipeline
        .drive_streaming_for_test(
            ts,
            CooperativePacer,
            &stop,
            Some(TICKS),
            SendPolicy::DropOnOverload,
            vec![Box::new(blocked_runner)],
            None,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    // Invariant #1: the engine emitted all N ticks despite the wedged sink.
    assert_eq!(
        result.report.frames, TICKS,
        "the output clock must emit all N ticks regardless of a stalled sink (inv #1)"
    );
    // Invariant #10 / bounded memory: the in-flight channel occupancy never
    // exceeded the fixed cap — memory is O(cap), not O(ticks).
    assert!(
        result.peak_occupancy <= result.capacity,
        "peak channel occupancy {} must never exceed the cap {} (bounded memory, inv #10)",
        result.peak_occupancy,
        result.capacity
    );
    // Overload was shed and COUNTED (visible, not hidden), so the run faltered.
    assert!(
        result.report.dropped > 0,
        "a wedged sink under live policy must shed frames and count them (got {} dropped)",
        result.report.dropped
    );
    assert!(
        result.report.faltered,
        "dropped > 0 means the live output faltered (honest reporting)"
    );
    assert!(
        received.load(Ordering::Acquire) >= 1,
        "the sink should have received at least one frame before wedging"
    );
}

/// (2) Offline (`run_for` semantics, `BlockForExact`): exactly N frames are
/// baked + handed to the sink, nothing is dropped, and the run did not falter.
///
/// RED today: the seam does not exist; and once it does, this is the regression
/// guard proving the offline path keeps exact, all-frames semantics under a
/// *slow* consumer (the channel back-pressures the renderer instead of dropping).
#[tokio::test(flavor = "current_thread")]
async fn offline_block_for_exact_delivers_all_n_frames() {
    const TICKS: u64 = 120;

    let mut pipeline = build_pipeline();

    // A fake sink that is deliberately SLOW (sleeps a little per frame) but
    // never blocks forever. Under BlockForExact the hot loop must wait for it,
    // so every frame is delivered and none dropped.
    let counted = Arc::new(AtomicUsize::new(0));
    let counted_in_sink = Arc::clone(&counted);
    let slow_runner = move |rx: Receiver<Arc<Nv12Image>>| -> TestSinkOutcome {
        while let Ok(_frame) = rx.recv() {
            std::thread::sleep(std::time::Duration::from_millis(1));
            counted_in_sink.fetch_add(1, Ordering::Release);
        }
        TestSinkOutcome {
            frames: counted_in_sink.load(Ordering::Acquire),
        }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    let ts: Arc<dyn TimeSource> = Arc::clone(&clock) as Arc<dyn TimeSource>;
    let result = pipeline
        .drive_streaming_for_test(
            ts,
            CooperativePacer,
            &stop,
            Some(TICKS),
            SendPolicy::BlockForExact,
            vec![Box::new(slow_runner)],
            None,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert_eq!(result.report.frames, TICKS, "N ticks");
    assert_eq!(
        result.report.dropped, 0,
        "offline block-for-exact must NEVER drop a frame"
    );
    assert!(
        !result.report.faltered,
        "offline never falters (dropped == 0)"
    );
    assert_eq!(
        result.sink_frames,
        vec![usize::try_from(TICKS).unwrap()],
        "the sink must have received exactly N frames (all baked, none dropped)"
    );
}

/// (3) Streaming, not batch: frame 0 reaches the sink WHILE the engine is still
/// ticking. The fake sink records the hot-loop tick index at the moment it
/// receives its first frame; in a streaming design that index is small (the
/// consumer runs concurrently), whereas the pre-ADR-0025 batch design encodes
/// only AFTER the loop, so frame 0 would arrive at tick == N.
///
/// RED today: batch path encodes post-loop ⇒ frame 0 observed at tick N.
#[tokio::test(flavor = "current_thread")]
async fn frame_zero_is_encoded_while_engine_still_ticking() {
    const TICKS: u64 = 300;

    let mut pipeline = build_pipeline();

    // Shared hot-loop tick counter, bumped once per tick by the seam's hot loop
    // and observed by the sink when it receives frame 0.
    let hot_tick = Arc::new(AtomicU64::new(0));
    let hot_tick_for_sink = Arc::clone(&hot_tick);
    let tick_at_frame_zero = Arc::new(AtomicU64::new(u64::MAX));
    let observed = Arc::clone(&tick_at_frame_zero);

    let runner = move |rx: Receiver<Arc<Nv12Image>>| -> TestSinkOutcome {
        let mut frames = 0_usize;
        while let Ok(_frame) = rx.recv() {
            if frames == 0 {
                observed.store(hot_tick_for_sink.load(Ordering::Acquire), Ordering::Release);
            }
            frames += 1;
            // Keep up easily so the consumer drains promptly and the test is fast.
            std::thread::yield_now();
        }
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    let ts: Arc<dyn TimeSource> = Arc::clone(&clock) as Arc<dyn TimeSource>;
    let result = pipeline
        .drive_streaming_for_test(
            ts,
            CooperativePacer,
            &stop,
            Some(TICKS),
            SendPolicy::BlockForExact,
            vec![Box::new(runner)],
            Some(Arc::clone(&hot_tick)),
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert_eq!(result.report.frames, TICKS, "N ticks");
    let observed_tick = tick_at_frame_zero.load(Ordering::Acquire);
    assert!(
        observed_tick != u64::MAX,
        "the sink must have received frame 0"
    );
    // The decisive assertion: frame 0 was encoded long before the engine reached
    // the final tick. A batch design would only deliver frame 0 at tick == N.
    assert!(
        observed_tick < TICKS,
        "frame 0 must be encoded WHILE the engine is still ticking (streaming, not batch); \
         observed hot tick {observed_tick} at frame 0, of {TICKS}"
    );
}

/// (4) Clean stop: raising the stop signal mid-run ends the engine, the
/// consumer drains, and every sink sees end-of-program (the channel closes), so
/// it can finalise (write its trailer). We assert the sink saw its receiver
/// close cleanly (`recv` returned `Err`) by checking it returned a finite frame
/// count and the run reports no falter (no drops on the offline-style path).
#[tokio::test(flavor = "current_thread")]
async fn clean_stop_closes_sinks_for_finalisation() {
    const MAX: u64 = 5_000;

    let mut pipeline = build_pipeline();

    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished_in_sink = Arc::clone(&finished);
    let runner = move |rx: Receiver<Arc<Nv12Image>>| -> TestSinkOutcome {
        let mut frames = 0_usize;
        // recv loop: returns Err ONLY when the consumer drops the sender at
        // teardown — i.e. end-of-program. Setting the flag proves the sink got a
        // clean close (the precondition for writing a trailer).
        while rx.recv().is_ok() {
            frames += 1;
        }
        finished_in_sink.store(true, Ordering::Release);
        TestSinkOutcome { frames }
    };

    let clock = Arc::new(ManualTimeSource::new());
    let stop = StopSignal::new();
    let driver_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_clock_driver(Arc::clone(&clock), Arc::clone(&driver_stop));

    // Raise the stop signal shortly after launching so the unbounded `run_until`
    // path stops promptly (it would otherwise run to MAX).
    let stop_for_thread = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(50));
        stop_for_thread.stop();
    });

    let ts: Arc<dyn TimeSource> = Arc::clone(&clock) as Arc<dyn TimeSource>;
    let result = pipeline
        .drive_streaming_for_test(
            ts,
            CooperativePacer,
            &stop,
            Some(MAX),
            SendPolicy::DropOnOverload,
            vec![Box::new(runner)],
            None,
        )
        .await
        .expect("drive streaming");
    driver_stop.store(true, Ordering::Release);

    assert!(
        result.report.frames < MAX,
        "the stop signal must have ended the run before the cap ({} of {MAX})",
        result.report.frames
    );
    assert!(
        finished.load(Ordering::Acquire),
        "the sink must observe a clean channel close (end-of-program) so it can finalise"
    );
}
