//! Node presentation discipline (DEV-C2 / ADR-0045 §2, display-out §8): the
//! pull-side frame chooser — a bounded 2–3-frame presentation queue drained
//! from the wait-free mailbox, a vblank predictor anchored on KMS flip
//! timestamps, and the pure "present the frame whose `wall_at(pts) +
//! link_offset` is closest to the predicted next vblank;
//! repeat-if-early/drop-if-late" decision — plus the flip-timestamp skew
//! telemetry. Everything here runs hardware-free over the scripted
//! `KmsBackend` mock and a scripted mono/wall clock pair (invariants #1 +
//! #10: the discipline is pure pull-side frame choice; nothing feeds back
//! into the engine).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multiview_core::time::Rational;
use multiview_core::wallclock::WallClockRef;
use multiview_output::display::{
    choose_frame, frame_mailbox, ConnectorDesc, ConnectorSelector, DisplayCanvas, DisplayError,
    DisplayModeInfo, DisplaySink, DisplaySinkConfig, FlipEvent, FrameChoice, HeadSetup, KmsBackend,
    ModeRequest, PresentQueue, PresentationClock, PresentationPlan, SubmitError, VblankPredictor,
    PRESENT_QUEUE_DEPTH,
};
use multiview_output::SharedEpoch;

/// The test refresh: exactly 50 Hz → an exact 20 ms vblank period.
const PERIOD_NS: i64 = 20_000_000;

/// The fixed wall−mono offset of the scripted clock pair (500 s).
const WALL_MINUS_MONO_NS: i64 = 500_000_000_000;

/// The link offset the scenario presents with (5 ms — consumed, not merely
/// recorded: deadlines are `wall_at(pts) + 5 ms`).
const LINK_OFFSET_NS: i64 = 5_000_000;

// ---------------------------------------------------------------------------
// choose_frame — the pure deadline-vs-vblank decision
// ---------------------------------------------------------------------------

#[test]
fn empty_queue_is_idle() {
    assert_eq!(choose_frame(&[], 1_000, PERIOD_NS), FrameChoice::Idle);
}

#[test]
fn the_deadline_closest_to_the_predicted_vblank_wins() {
    // V = 100 ms; deadlines at 88 ms and 104 ms: the 104 ms frame is closer
    // (4 ms vs 12 ms) and presents; the 88 ms frame is the late skip.
    let v = 100_000_000;
    let deadlines = [88_000_000, 104_000_000];
    assert_eq!(
        choose_frame(&deadlines, v, PERIOD_NS),
        FrameChoice::Present { index: 1 }
    );
}

#[test]
fn an_equidistant_tie_prefers_the_newer_frame() {
    // Both 10 ms away: newest content wins (drop the stale one, never show
    // older content when newer is equally placed).
    let v = 100_000_000;
    let deadlines = [90_000_000, 110_000_000];
    assert_eq!(
        choose_frame(&deadlines, v, PERIOD_NS),
        FrameChoice::Present { index: 1 }
    );
}

#[test]
fn a_frame_more_than_half_a_period_early_repeats() {
    // V = 100 ms, the only frame is due at 112 ms: 12 ms early > half the
    // 20 ms period — it belongs to the NEXT vblank; repeat the current glass
    // (KMS repeats the framebuffer for free).
    let v = 100_000_000;
    assert_eq!(
        choose_frame(&[112_000_000], v, PERIOD_NS),
        FrameChoice::RepeatEarly
    );
}

#[test]
fn exactly_half_a_period_early_presents() {
    // The boundary case: d − V == period/2 exactly is NOT "nearer the next
    // vblank" — it presents now (2·(d−V) > period is the repeat test).
    let v = 100_000_000;
    assert_eq!(
        choose_frame(&[110_000_000], v, PERIOD_NS),
        FrameChoice::Present { index: 0 }
    );
}

#[test]
fn a_late_only_frame_still_presents_catching_up() {
    // The only frame is 3 periods late. Presenting it keeps content moving
    // (the output never falters under drift); skew telemetry shows the slip.
    let v = 100_000_000;
    assert_eq!(
        choose_frame(&[40_000_000], v, PERIOD_NS),
        FrameChoice::Present { index: 0 }
    );
}

#[test]
fn a_degenerate_period_presents_the_newest_frame() {
    // No usable vblank period (degenerate timing): no discipline is possible;
    // newest-wins keeps the glass live.
    assert_eq!(
        choose_frame(&[1, 2, 3], 0, 0),
        FrameChoice::Present { index: 2 }
    );
}

// ---------------------------------------------------------------------------
// VblankPredictor — exact-rational period, flip-anchored prediction
// ---------------------------------------------------------------------------

#[test]
fn the_period_is_derived_exactly_from_the_rational_refresh() {
    assert_eq!(
        VblankPredictor::new(Rational::new(50, 1)).period_ns(),
        20_000_000
    );
    assert_eq!(
        VblankPredictor::new(Rational::new(60, 1)).period_ns(),
        16_666_667,
        "60 Hz rounds half away from zero — never float math"
    );
    assert_eq!(
        VblankPredictor::new(Rational::new(60_000, 1_001)).period_ns(),
        16_683_333,
        "NTSC 1001 stays an exact rational division"
    );
}

#[test]
fn prediction_needs_a_flip_anchor() {
    let predictor = VblankPredictor::new(Rational::new(50, 1));
    assert_eq!(
        predictor.predicted_next_ns(1_000_000_000),
        None,
        "before the first KMS flip timestamp there is no vblank phase"
    );
}

#[test]
fn prediction_is_the_next_grid_instant_after_now() {
    let mut predictor = VblankPredictor::new(Rational::new(50, 1));
    predictor.on_flip(1_000_000_000);
    // Immediately after the flip: the next vblank is one period out.
    assert_eq!(
        predictor.predicted_next_ns(1_000_000_000),
        Some(1_020_000_000)
    );
    // 31 ms later (one whole vblank passed without a commit): the prediction
    // free-runs forward on the SAME grid — phase is kept, never reset.
    assert_eq!(
        predictor.predicted_next_ns(1_031_000_000),
        Some(1_040_000_000)
    );
}

#[test]
fn a_new_flip_re_anchors_the_grid() {
    let mut predictor = VblankPredictor::new(Rational::new(50, 1));
    predictor.on_flip(1_000_000_000);
    // The kernel's next flip lands 100 µs late (real scanout drift): the grid
    // re-anchors on the measured timestamp, so error never accumulates.
    predictor.on_flip(1_020_100_000);
    assert_eq!(
        predictor.predicted_next_ns(1_020_100_000),
        Some(1_040_100_000)
    );
}

#[test]
fn a_degenerate_refresh_never_predicts() {
    let mut predictor = VblankPredictor::new(Rational::new(0, 1));
    predictor.on_flip(1_000_000_000);
    assert_eq!(predictor.period_ns(), 0);
    assert_eq!(predictor.predicted_next_ns(1_000_000_001), None);
}

// ---------------------------------------------------------------------------
// PresentQueue — the bounded pull-side frame queue
// ---------------------------------------------------------------------------

#[test]
fn the_queue_is_bounded_and_drops_oldest() {
    let mut queue: PresentQueue<&'static str> = PresentQueue::new();
    assert!(queue.is_empty());
    assert!(!queue.push("a", 1, 10));
    assert!(!queue.push("b", 2, 20));
    assert!(!queue.push("c", 3, 30));
    assert_eq!(queue.len(), PRESENT_QUEUE_DEPTH);
    // A fourth frame overflows: the OLDEST is dropped (newest wins), and the
    // overflow is reported so the sink can count it.
    assert!(queue.push("d", 4, 40));
    assert_eq!(queue.len(), PRESENT_QUEUE_DEPTH);
    let (frame, seq, pts) = queue.entry(0).expect("entry 0");
    assert_eq!((*frame, seq, pts), ("b", 2, 20));
}

#[test]
fn pop_through_consumes_the_chosen_frame_and_counts_late_skips() {
    let mut queue: PresentQueue<&'static str> = PresentQueue::new();
    queue.push("a", 1, 10);
    queue.push("b", 2, 20);
    queue.push("c", 3, 30);
    // Present index 1: "a" is the late skip, "b" is consumed, "c" remains.
    assert_eq!(queue.pop_through(1), 1);
    assert_eq!(queue.len(), 1);
    let (frame, seq, pts) = queue.entry(0).expect("the remaining entry");
    assert_eq!((*frame, seq, pts), ("c", 3, 30));
}

#[test]
fn deadlines_are_pts_plus_link_offset_through_the_epoch() {
    let mut queue: PresentQueue<&'static str> = PresentQueue::new();
    queue.push("a", 1, 1_000);
    queue.push("b", 2, 2_000);
    // Epoch: wall = 7_000 + pts (1 GHz media rate → exact ns identity).
    let epoch = WallClockRef::new(7_000, 0, Rational::new(1_000_000_000, 1));
    assert_eq!(
        queue.deadlines(epoch, 50),
        vec![8_050, 9_050],
        "deadline = wall_at(pts) + link_offset, exact integer ns"
    );
}

// ---------------------------------------------------------------------------
// The mailbox carries the presentation timestamp with the frame
// ---------------------------------------------------------------------------

#[test]
fn the_mailbox_stamps_the_pts_atomically_with_the_frame() {
    let (publisher, reader) = frame_mailbox::<&'static str>();
    // `publish_at` carries the presentation timestamp alongside the frame; the
    // 1-arg `publish` (pts 0) stays the engine-agnostic mailbox primitive the
    // DEV-B1 sink already uses.
    publisher.publish_at("a", 111);
    publisher.publish_at("b", 222);
    let (frame, seq) = reader.latest().expect("a frame");
    assert_eq!(*frame, "b");
    assert_eq!(seq, 2);
    assert_eq!(
        frame.pts_ns(),
        222,
        "the pts travels INSIDE the slot value — never frame N with pts N+1"
    );
}

// ---------------------------------------------------------------------------
// The scripted backend + clock for sink-level discipline tests
// ---------------------------------------------------------------------------

/// A scripted mono/wall clock pair: `mono` is the test-driven virtual
/// `CLOCK_MONOTONIC` (the KMS flip-timestamp domain); wall = mono + the fixed
/// offset (the epoch domain).
struct ScriptedClock {
    mono: Arc<AtomicI64>,
}

impl PresentationClock for ScriptedClock {
    fn now_pair(&mut self) -> (i64, i64) {
        let mono = self.mono.load(Ordering::Acquire);
        (mono, mono.saturating_add(WALL_MINUS_MONO_NS))
    }
}

/// A hardware-free `KmsBackend` for presentation tests: successful submits
/// owe one flip event stamped on the exact 20 ms vblank grid, and owed flips
/// are released only once the scripted mono clock reaches their timestamp
/// (a flip completes AT its vblank, never before).
struct PresentMockBackend {
    connectors: Vec<ConnectorDesc>,
    mono: Arc<AtomicI64>,
    /// Owed flip timestamps (mono ns), released when due.
    owed: VecDeque<i64>,
    /// Scripted results for successive submits (`None` = Ok).
    submit_results: Mutex<VecDeque<Option<SubmitError>>>,
    /// Tags of successfully submitted frames (first luma byte).
    submits: Arc<Mutex<Vec<u8>>>,
    /// `wait_events` entry counter: the sync point proving the sink completed
    /// full iterations (wait → drain mailbox → decide).
    wait_calls: Arc<Mutex<u64>>,
    frame_counter: u64,
}

impl PresentMockBackend {
    fn new(mono: Arc<AtomicI64>) -> Self {
        Self {
            connectors: vec![connected_dp1_50hz()],
            mono,
            owed: VecDeque::new(),
            submit_results: Mutex::new(VecDeque::new()),
            submits: Arc::new(Mutex::new(Vec::new())),
            wait_calls: Arc::new(Mutex::new(0)),
            frame_counter: 0,
        }
    }

    /// The next vblank-grid instant strictly after the current mono clock.
    fn next_grid_after_now(&self) -> i64 {
        let now = self.mono.load(Ordering::Acquire);
        (now.div_euclid(PERIOD_NS) + 1).saturating_mul(PERIOD_NS)
    }
}

impl KmsBackend for PresentMockBackend {
    fn probe_connectors(&mut self) -> Result<Vec<ConnectorDesc>, DisplayError> {
        Ok(self.connectors.clone())
    }

    fn validate_setup(&mut self, _setup: &HeadSetup) -> Result<(), DisplayError> {
        Ok(())
    }

    fn apply_modeset(&mut self, _setup: &HeadSetup) -> Result<(), DisplayError> {
        Ok(())
    }

    fn submit_frame(&mut self, frame: &dyn DisplayCanvas) -> Result<(), SubmitError> {
        let scripted = self.submit_results.lock().unwrap().pop_front().flatten();
        match scripted {
            None => {
                self.submits
                    .lock()
                    .unwrap()
                    .push(frame.y_plane().first().copied().unwrap_or(0));
                let ts = self.next_grid_after_now();
                self.owed.push_back(ts);
                Ok(())
            }
            Some(SubmitError::Busy) => {
                // A real EBUSY means the kernel still has an outstanding flip:
                // it WILL complete — owe one event so the scenario matches the
                // kernel contract.
                let ts = self.next_grid_after_now();
                self.owed.push_back(ts);
                Err(SubmitError::Busy)
            }
            Some(err) => Err(err),
        }
    }

    fn wait_events(&mut self, timeout: Duration) -> Result<Vec<FlipEvent>, DisplayError> {
        *self.wait_calls.lock().unwrap() += 1;
        // Release every owed flip whose vblank the virtual clock has reached.
        let now = self.mono.load(Ordering::Acquire);
        let mut events = Vec::new();
        while let Some(&ts) = self.owed.front() {
            if ts > now {
                break;
            }
            self.owed.pop_front();
            self.frame_counter += 1;
            events.push(FlipEvent {
                crtc_frame: u32::try_from(self.frame_counter).unwrap_or(u32::MAX),
                timestamp: Duration::from_nanos(u64::try_from(ts).unwrap_or(0)),
            });
        }
        if events.is_empty() {
            // Model the bounded poll wait without hot-spinning the test host.
            std::thread::sleep(timeout.min(Duration::from_millis(1)));
        }
        Ok(events)
    }
}

/// A 2x2 NV12 canvas whose every luma byte is `tag` (frame identity).
#[derive(Debug)]
struct TestCanvas {
    y: [u8; 4],
    uv: [u8; 2],
}

impl TestCanvas {
    fn tagged(tag: u8) -> Self {
        Self {
            y: [tag; 4],
            uv: [128; 2],
        }
    }
}

impl DisplayCanvas for TestCanvas {
    fn width(&self) -> u32 {
        2
    }
    fn height(&self) -> u32 {
        2
    }
    fn y_plane(&self) -> &[u8] {
        &self.y
    }
    fn uv_plane(&self) -> &[u8] {
        &self.uv
    }
}

/// A 1920x1080 mode at exactly 50/1 Hz (100 MHz pixel clock over 2000x1000
/// totals): the exact 20 ms vblank period the scenario is scripted on.
fn connected_dp1_50hz() -> ConnectorDesc {
    ConnectorDesc {
        name: "DP-1".to_owned(),
        connected: true,
        modes: vec![DisplayModeInfo {
            width: 1920,
            height: 1080,
            clock_khz: 100_000,
            hsync_start: 1930,
            hsync_end: 1960,
            htotal: 2000,
            vsync_start: 990,
            vsync_end: 995,
            vtotal: 1000,
            hsync_positive: true,
            vsync_positive: true,
            preferred: true,
        }],
    }
}

fn presentation_sink_config(epoch: &SharedEpoch, mono: &Arc<AtomicI64>) -> DisplaySinkConfig {
    DisplaySinkConfig {
        output_id: "out-node-head".to_owned(),
        connector: ConnectorSelector::Auto,
        mode: ModeRequest::Auto,
        forced_mode: None,
        engine_cadence: Some(Rational::new(50, 1)),
        poll_interval: Duration::from_millis(1),
        presentation: Some(PresentationPlan {
            epoch: epoch.clone(),
            link_offset_ns: LINK_OFFSET_NS,
            clock: Box::new(ScriptedClock {
                mono: Arc::clone(mono),
            }),
        }),
    }
}

/// Spin until `pred` is true or ~5 s elapse (the sink polls at 1 ms; success
/// is near-immediate — the bound only guards CI).
fn wait_until(pred: impl Fn() -> bool) {
    for _ in 0..5_000 {
        if pred() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(pred(), "condition not reached within the test bound");
}

/// Wait for the sink to complete at least two further full loop iterations
/// (`wait_events` → drain mailbox → decide), proving it has observed everything
/// published before this call.
fn two_full_iterations(wait_calls: &Arc<Mutex<u64>>) {
    let start = *wait_calls.lock().unwrap();
    wait_until(|| *wait_calls.lock().unwrap() >= start + 3);
}

/// The epoch the scenario presents against: `wall_at(pts) = WALL_MINUS_MONO +
/// pts` (1 GHz media rate), so a frame's scheduled MONO instant is simply
/// `pts + LINK_OFFSET_NS` — the script reads directly in the mono domain.
fn scenario_epoch() -> WallClockRef {
    WallClockRef::new(WALL_MINUS_MONO_NS, 0, Rational::new(1_000_000_000, 1))
}

// ---------------------------------------------------------------------------
// Sink-level discipline: the full pull-side loop over the scripted seam
// ---------------------------------------------------------------------------

/// The DEV-C2 acceptance scenario, end to end on the scripted seam:
///
/// 1. the first frame lights the pipe undisciplined (no vblank anchor yet);
/// 2. a frame whose deadline is more than half a period beyond the next
///    vblank is NOT committed (repeat-if-early — KMS repeats the glass);
/// 3. once its vblank approaches, it presents, and the flip-timestamp skew
///    (flip ts − scheduled instant) is exported;
/// 4. with two frames queued, the one nearest the predicted vblank presents
///    and the stale one is dropped (drop-if-late);
/// 5. the epoch was set ONCE before start and never refreshed — every
///    disciplined present above ran on the held map (free-run: a lost
///    controller feed only stops updates; presentation never falters).
#[test]
fn the_sink_presents_by_deadline_against_the_predicted_vblank() {
    let mono = Arc::new(AtomicI64::new(1_000_000_000));
    let epoch = SharedEpoch::new();
    // Set ONCE, before the sink starts; never touched again (step 5).
    epoch.set(scenario_epoch());
    let backend = PresentMockBackend::new(Arc::clone(&mono));
    let submits = Arc::clone(&backend.submits);
    let wait_calls = Arc::clone(&backend.wait_calls);

    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, presentation_sink_config(&epoch, &mono))
            .expect("the sink starts");
    let stats = handle.stats();

    // -- 1. Light up: no flip anchor exists yet → undisciplined latest-wins.
    publisher.publish_at(TestCanvas::tagged(b'A'), 1_000_000_000);
    wait_until(|| stats.snapshot().commits == 1);
    assert_eq!(stats.snapshot().undisciplined_presents, 1);
    // Release A's flip (vblank 1.020 s) and anchor the predictor.
    mono.store(1_021_000_000, Ordering::Release);
    wait_until(|| stats.snapshot().flips == 1);

    // -- 2. Repeat-if-early: B is due at mono 1.062 s (pts 1.057 s + 5 ms
    //       link offset). The next vblank is 1.040 s: B is 22 ms early —
    //       nearer the FOLLOWING vblank — so nothing commits.
    publisher.publish_at(TestCanvas::tagged(b'B'), 1_057_000_000);
    two_full_iterations(&wait_calls);
    two_full_iterations(&wait_calls);
    assert_eq!(
        stats.snapshot().commits,
        1,
        "an early frame must NOT be committed (KMS repeats the glass for free)"
    );

    // -- 3. Its vblank approaches: at mono 1.052 s the predicted vblank is
    //       1.060 s and B's deadline (1.062 s) is 2 ms past it — within half
    //       a period → present. The commit's flip is owed at the 1.060 s
    //       vblank (not yet released: the pipe stays in flight).
    mono.store(1_052_000_000, Ordering::Release);
    wait_until(|| stats.snapshot().commits == 2);
    assert_eq!(stats.snapshot().presented, 1, "one disciplined present");

    // -- 4. Drop-if-late: queue C (due 1.068 s) and D (due 1.088 s) while
    //       B's flip is still in flight (no decision can land in between),
    //       then advance to 1.075 s: B's flip drains (vblank 1.060 s; skew =
    //       1.060 − 1.062 = −2 ms) and the next predicted vblank is
    //       1.080 s — C is 12 ms away, D is 8 ms away → D presents, C is
    //       dropped late, never submitted.
    publisher.publish_at(TestCanvas::tagged(b'C'), 1_063_000_000);
    two_full_iterations(&wait_calls);
    publisher.publish_at(TestCanvas::tagged(b'D'), 1_083_000_000);
    two_full_iterations(&wait_calls);
    assert_eq!(
        stats.snapshot().commits,
        2,
        "no decision lands while B's flip is in flight"
    );
    mono.store(1_075_000_000, Ordering::Release);
    wait_until(|| stats.snapshot().flips == 2);
    assert_eq!(
        stats.snapshot().last_flip_skew_ns,
        -2_000_000,
        "B's flip-timestamp skew: flip (1.060 s) − scheduled (1.062 s)"
    );
    assert!(stats.snapshot().max_flip_skew_abs_ns >= 2_000_000);
    wait_until(|| stats.snapshot().commits == 3);
    let snapshot = stats.snapshot();
    assert_eq!(snapshot.presented, 2);
    assert_eq!(snapshot.late_skips, 1, "C was dropped as late");
    assert_eq!(
        *submits.lock().unwrap(),
        vec![b'A', b'B', b'D'],
        "C must never reach the device"
    );
    // D's flip lands on the 1.080 s vblank: skew = 1.080 − 1.088 = −8 ms.
    mono.store(1_081_000_000, Ordering::Release);
    wait_until(|| stats.snapshot().flips == 3);
    assert_eq!(stats.snapshot().last_flip_skew_ns, -8_000_000);

    // -- 5. Free-run proof: the epoch cell still holds the ONE map set before
    //       start — every disciplined present above used the held epoch.
    assert_eq!(epoch.get(), Some(scenario_epoch()));
    handle.stop();
}

/// EBUSY retention: a Busy answer must keep the chosen frame QUEUED (it is
/// the retry candidate after the pending flip drains) — never popped, never
/// skipped.
#[test]
fn ebusy_keeps_the_chosen_frame_as_the_retry_candidate() {
    let mono = Arc::new(AtomicI64::new(1_000_000_000));
    let epoch = SharedEpoch::new();
    epoch.set(scenario_epoch());
    let backend = PresentMockBackend::new(Arc::clone(&mono));
    let submits = Arc::clone(&backend.submits);
    // First submit (frame A, undisciplined light-up) answers EBUSY.
    backend
        .submit_results
        .lock()
        .unwrap()
        .push_back(Some(SubmitError::Busy));

    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, presentation_sink_config(&epoch, &mono))
            .expect("the sink starts");
    let stats = handle.stats();

    publisher.publish_at(TestCanvas::tagged(b'A'), 1_000_000_000);
    wait_until(|| stats.snapshot().busy_conflations == 1);
    assert_eq!(stats.snapshot().commits, 0, "EBUSY is not a commit");
    // The unaccounted-for kernel flip drains at its vblank; the SAME frame A
    // is then committed.
    mono.store(1_021_000_000, Ordering::Release);
    wait_until(|| stats.snapshot().commits == 1);
    assert_eq!(*submits.lock().unwrap(), vec![b'A']);
    handle.stop();
}

/// No epoch was EVER published (e.g. a node before its first local epoch
/// sample): presentation falls back to undisciplined latest-wins — the
/// output never falters waiting for timing.
#[test]
fn without_any_epoch_the_sink_still_presents_latest_wins() {
    let mono = Arc::new(AtomicI64::new(1_000_000_000));
    let epoch = SharedEpoch::new(); // never set
    let backend = PresentMockBackend::new(Arc::clone(&mono));
    let wait_calls = Arc::clone(&backend.wait_calls);

    let (handle, publisher) =
        DisplaySink::start::<TestCanvas, _>(backend, presentation_sink_config(&epoch, &mono))
            .expect("the sink starts");
    let stats = handle.stats();

    publisher.publish_at(TestCanvas::tagged(b'A'), 1_000_000_000);
    wait_until(|| stats.snapshot().commits == 1);
    // Release A's flip so the pipe idles again.
    mono.store(1_021_000_000, Ordering::Release);
    wait_until(|| stats.snapshot().flips == 1);
    publisher.publish_at(TestCanvas::tagged(b'B'), 1_037_000_000);
    two_full_iterations(&wait_calls);
    wait_until(|| stats.snapshot().commits == 2);
    assert_eq!(
        stats.snapshot().undisciplined_presents,
        2,
        "no epoch ⇒ every present is honestly undisciplined"
    );
    handle.stop();
}
