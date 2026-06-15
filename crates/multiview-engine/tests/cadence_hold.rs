//! Output clock holds **wall-clock cadence under overload** (ADR-T018) —
//! invariant #1's true meaning, proven deterministically.
//!
//! The drive loop composes one fresh frame per loop iteration. On a
//! CPU/GPU-contended host a `compose()` can overrun the tick budget; the prior
//! loop then free-ran at *composition* speed, emitting each index late, so
//! media-time fell progressively behind wall-clock (the frigate 84-minute-lag
//! incident). ADR-T018 makes the emitted tick **track wall-clock** by re-emitting
//! the held last-good frame for each whole tick-period that has already elapsed
//! (invariant #2 lifted to the output clock), under a fresh strictly-increasing
//! pts (invariant #3), bounded per iteration (no spin) — so the output holds
//! exactly 1.0× cadence rather than slipping.
//!
//! These tests drive the REAL [`EngineRuntime`] loop with a [`ManualTimeSource`]
//! and [`CooperativePacer`] (zero real sleeps, fully deterministic), simulating a
//! slow compositor by advancing the time source inside the per-tick control hook
//! (i.e. "by the time this fresh compose finished, N tick-periods had elapsed").
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::as_conversions,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_core::color::ColorInfo;
use multiview_core::layout::{Canvas, Cell, FitMode, Layout};
use multiview_core::time::Rational;
use multiview_engine::clock::ManualTimeSource;
use multiview_engine::{
    CompositedFrame, CompositorDrive, CooperativePacer, EnginePublisher, EngineRuntime,
    OutputClock, StopSignal, TimeSource, MAX_REPEATS_PER_TICK,
};
use multiview_framestore::TileStore;

/// Independent i128 oracle for `out_pts = f(tick)` (half-away-from-zero), NOT via
/// the clock's `rescale`/`from_tick`.
fn oracle_pts_ns(tick: i64, cadence: Rational) -> i64 {
    let numerator: i128 = i128::from(tick) * 1_000_000_000_i128 * i128::from(cadence.den);
    let denominator: i128 = i128::from(cadence.num);
    let q = numerator / denominator;
    let r = numerator % denominator;
    let rounded = if r * 2 >= denominator { q + 1 } else { q };
    i64::try_from(rounded).expect("oracle pts fits in i64")
}

fn resolved_color() -> ColorInfo {
    ColorInfo::default().resolve_defaults(1920, 1080)
}

/// A one-cell drive over a single never-fed store (every tile rides `NoSignal` —
/// a valid frame is still produced every tick).
fn build_drive(w: u32, h: u32) -> CompositorDrive<Nv12Image> {
    let mut stores = HashMap::new();
    stores.insert(
        "cam-a".to_owned(),
        Arc::new(TileStore::<Nv12Image>::with_defaults("cam-a")),
    );
    CompositorDrive::new(
        Arc::new(Layout {
            name: "cadence".to_owned(),
            canvas: Canvas {
                width: w,
                height: h,
                fps_num: 25,
                fps_den: 1,
            },
            cells: vec![Cell {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                z: 0,
                fit: FitMode::Contain,
                source: Some("cam-a".to_owned()),
                ..Cell::default()
            }],
        }),
        stores,
        Nv12Image::solid(w, h, 16, 128, 128, resolved_color()).unwrap(),
        CanvasColor::default(),
        LinearRgba::TRANSPARENT,
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overloaded_compositor_holds_wall_clock_cadence_by_repeating_not_slipping() {
    // Each fresh compose "takes" OVERRUN tick-periods (simulated by advancing the
    // manual clock that much inside the control hook, which runs once per fresh
    // compose). A correct loop emits OVERRUN-1 last-good repeats + 1 fresh per
    // iteration, so the published index tracks wall-clock at 1.0×. A slipping loop
    // (the bug) would emit one late fresh composite per OVERRUN periods → ~0.33×.
    const OVERRUN: i64 = 3;
    const TICKS: u64 = 300;
    // An EXACT-period cadence (25 fps → 40,000,000 ns/tick, no rounding): the
    // harness advances the manual clock by whole periods, so `advance(k·period)`
    // lands precisely on `pts_at(k)`. (A non-integer-period cadence like 29.97
    // would drift the harness ~1 ns per tick vs the clock's exact deadlines — a
    // test artifact, not an engine bug; the clock's NTSC exactness is covered by
    // the clock unit tests + the FPS_60 soak.)
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );
    let seed = runtime.seed_nanos();

    // Capture every published (index, pts) so we can prove contiguity + the oracle.
    let log = Arc::new(Mutex::new(Vec::<(u64, i64)>::new()));
    let log_s = Arc::clone(&log);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s
            .lock()
            .unwrap()
            .push((f.tick.index, f.pts().as_nanos()));
        f.tick.index
    };

    // control hook = one fresh compose just finished, having taken OVERRUN periods.
    let ts_ctl = Arc::clone(&time_source);
    let fresh_calls = Arc::new(AtomicU64::new(0));
    let fresh_ctl = Arc::clone(&fresh_calls);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        fresh_ctl.fetch_add(1, Ordering::AcqRel);
        ts_ctl.advance(Duration::from_nanos((OVERRUN * period) as u64));
    };

    let stop = StopSignal::new();
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            TICKS,
            state_of,
            |_f| None::<u64>,
            control,
        )
        .await
        .expect("run completes");

    assert_eq!(outcome.ticks, TICKS, "emitted exactly the requested ticks");
    let repeated = runtime.frames_repeated();
    let fresh = fresh_calls.load(Ordering::Acquire);

    // 1. Some ticks were REPEATS (the cadence-hold fired) and fresh composites are
    //    strictly fewer than emitted ticks — we did NOT compose every tick.
    assert!(repeated > 0, "overload must produce last-good repeats");
    assert_eq!(
        repeated + fresh,
        TICKS,
        "every emitted tick is either a fresh composite or a last-good repeat"
    );
    assert!(
        fresh < TICKS,
        "fewer fresh composites than emitted ticks under overload"
    );

    // 2. The published sequence is CONTIGUOUS (no muxer PTS gap) and every pts is
    //    the exact independent oracle (fresh OR repeat — repeats carry a fresh pts).
    let log = log.lock().unwrap();
    assert_eq!(log.len() as u64, TICKS);
    for (i, &(index, pts)) in log.iter().enumerate() {
        assert_eq!(index, i as u64, "published indices contiguous, no gap/dup");
        assert_eq!(
            pts,
            oracle_pts_ns(i as i64, cadence),
            "tick {i} pts must equal the oracle (repeats re-stamp, never reuse a pts)"
        );
    }

    // 3. The CADENCE HELD: the last emitted index tracks wall-clock (≈ now/period),
    //    NOT compose-speed. The slipping bug would leave last_index ≈ (now/period)/OVERRUN.
    let now = time_source.now_nanos();
    let wall_index = (now - seed) / period;
    let last_index = log.last().unwrap().0 as i64;
    assert!(
        last_index + 4 >= wall_index,
        "output slipped behind wall-clock: last index {last_index}, wall index {wall_index} \
         (a holding-cadence loop keeps these within a tick or two; slipping falls ~{OVERRUN}× behind)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_mid_tick_wake_does_not_run_ahead_or_repeat() {
    // The D1 guard: on a HEALTHY box every wake lands a fraction of a period past
    // the due deadline (ordinary scheduler jitter). The loop must emit the DUE
    // tick fresh — never round the fractional elapsed up to the next index (which
    // would compose a frame before its wall-clock deadline = run ahead) and never
    // spuriously repeat. Wakes here sit 0.6 of a period late, every tick.
    const TICKS: u64 = 100;
    // An EXACT-period cadence (25 fps → 40,000,000 ns/tick, no rounding): the
    // harness advances the manual clock by whole periods, so `advance(k·period)`
    // lands precisely on `pts_at(k)`. (A non-integer-period cadence like 29.97
    // would drift the harness ~1 ns per tick vs the clock's exact deadlines — a
    // test artifact, not an engine bug; the clock's NTSC exactness is covered by
    // the clock unit tests + the FPS_60 soak.)
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );

    // Prime a 0.6-period offset so every pacer wake is mid-tick-late but < 1 period.
    time_source.advance(Duration::from_nanos((period * 6 / 10) as u64));

    let log = Arc::new(Mutex::new(Vec::<u64>::new()));
    let log_s = Arc::clone(&log);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s.lock().unwrap().push(f.tick.index);
        f.tick.index
    };

    // Each fresh compose advances exactly one period → steady healthy 1.0× pacing,
    // but always 0.6 period late relative to the deadline (the priming offset).
    let ts_ctl = Arc::clone(&time_source);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        ts_ctl.advance(Duration::from_nanos(period as u64));
    };

    let stop = StopSignal::new();
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            TICKS,
            state_of,
            |_f| None::<u64>,
            control,
        )
        .await
        .expect("run completes");

    assert_eq!(outcome.ticks, TICKS);
    // ZERO repeats on a healthy system — a sub-period late wake is not overload.
    assert_eq!(
        runtime.frames_repeated(),
        0,
        "a sub-period-late wake must not trigger a cadence repeat (no run-ahead, no slip)"
    );
    // Exactly one fresh frame per tick, in order, never running ahead of the clock.
    let log = log.lock().unwrap();
    assert_eq!(log.len() as u64, TICKS);
    for (i, &index) in log.iter().enumerate() {
        assert_eq!(index, i as u64, "exactly the due tick each time, in order");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_time_jump_resyncs_with_bounded_repeats_and_no_spin() {
    // A PATHOLOGICAL one-off jump (multi-second deschedule / VM pause / migration):
    // CLOCK_MONOTONIC leaps thousands of periods between two composes. The loop
    // must NOT emit thousands of last-good repeats (a burst that floods the muxer)
    // nor spin — it emits at most MAX_REPEATS_PER_TICK repeats, then RESYNCS the
    // counter to wall-clock in one step (skip_to), accepting one bounded PTS gap.
    const HUGE: i64 = 10_000;
    let ticks: u64 = u64::from(MAX_REPEATS_PER_TICK) + 16;
    // An EXACT-period cadence (25 fps → 40,000,000 ns/tick, no rounding): the
    // harness advances the manual clock by whole periods, so `advance(k·period)`
    // lands precisely on `pts_at(k)`. (A non-integer-period cadence like 29.97
    // would drift the harness ~1 ns per tick vs the clock's exact deadlines — a
    // test artifact, not an engine bug; the clock's NTSC exactness is covered by
    // the clock unit tests + the FPS_60 soak.)
    let cadence = Rational::new(25, 1);
    let period = oracle_pts_ns(1, cadence);

    let time_source = Arc::new(ManualTimeSource::new());
    let ts: Arc<dyn TimeSource> = time_source.clone();
    let publisher: Arc<EnginePublisher<u64, u64>> = Arc::new(EnginePublisher::new(8));
    let mut runtime = EngineRuntime::new(
        OutputClock::new(cadence).unwrap(),
        build_drive(16, 16),
        ts,
        CooperativePacer,
    );

    let log = Arc::new(Mutex::new(Vec::<i64>::new()));
    let log_s = Arc::clone(&log);
    let state_of = move |f: &CompositedFrame| -> u64 {
        log_s.lock().unwrap().push(f.pts().as_nanos());
        f.tick.index
    };

    // Two warmup composes advance one period each; the THIRD jumps HUGE periods.
    let ts_ctl = Arc::clone(&time_source);
    let calls = Arc::new(AtomicU64::new(0));
    let calls_ctl = Arc::clone(&calls);
    let control = move |_d: &mut CompositorDrive<Nv12Image>| {
        let n = calls_ctl.fetch_add(1, Ordering::AcqRel);
        let mult = if n == 2 { HUGE } else { 1 };
        ts_ctl.advance(Duration::from_nanos((mult * period) as u64));
    };

    let stop = StopSignal::new();
    let outcome = runtime
        .run_for_with_control(
            publisher.as_ref(),
            &stop,
            ticks,
            state_of,
            |_f| None::<u64>,
            control,
        )
        .await
        .expect("run completes without spinning");

    assert_eq!(
        outcome.ticks, ticks,
        "the run terminated (no unbounded spin)"
    );
    // BOUNDED repeats: the jump produced at most one cap's worth of last-good
    // repeats — NOT ~HUGE of them.
    let repeated = runtime.frames_repeated();
    assert!(
        repeated <= u64::from(MAX_REPEATS_PER_TICK),
        "a huge jump must emit at most MAX_REPEATS_PER_TICK ({MAX_REPEATS_PER_TICK}) repeats, got {repeated}"
    );
    assert!(repeated > 0, "the jump did trigger the cadence-hold path");

    // RESYNCED to wall-clock: some emitted tick's pts is out near the jump target,
    // proving skip_to jumped the counter rather than crawling there frame by frame.
    let log = log.lock().unwrap();
    let max_pts = *log.iter().max().unwrap();
    let resync_floor = oracle_pts_ns(HUGE / 2, cadence); // generously past the cap region
    assert!(
        max_pts >= resync_floor,
        "after a huge jump the clock must resync near wall-clock (max pts {max_pts} < floor {resync_floor})"
    );
    // PTS strictly increasing throughout (the resync is a forward jump, not a rewind).
    for w in log.windows(2) {
        assert!(
            w[1] > w[0],
            "pts must be strictly increasing across the resync"
        );
    }
}
