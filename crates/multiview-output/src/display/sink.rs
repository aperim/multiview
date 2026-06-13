//! The display sink: startup (probe → mode-select → `TEST_ONLY` validate →
//! the one `ALLOW_MODESET` modeset) on the caller's thread, then a dedicated
//! flip-loop thread that owns the device for the rest of the run
//! (ADR-0044 §1).
//!
//! The loop is generic over [`KmsBackend`], so its entire behaviour —
//! conflation, EBUSY handling, no-new-frame repeat, modeset discipline — is
//! CI-tested over a scripted mock; only the [`super::kms`] backend touches
//! hardware. The engine side holds just a [`FramePublisher`] (wait-free); the
//! sink can wedge, crash, or stall without ever back-pressuring the engine
//! (invariants #1 + #10) — its failure mode is a frozen monitor showing the
//! last framebuffer while program output continues untouched.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use super::canvas::DisplayCanvas;
use super::device::{ConnectorSelector, DisplayError, HeadSetup, KmsBackend, SubmitError};
use super::hotplug::ReprobeFlag;
use super::mailbox::{frame_mailbox, FramePublisher, FrameReader, MailboxFrame};
use super::mode::{select_mode, ForcedMode, ModeRequest, SelectedMode};
use super::present::{choose_frame, FrameChoice, PresentQueue, PresentationPlan, VblankPredictor};
use super::FlipDriver;
use multiview_core::time::Rational;

/// Configuration for one display sink (one connector = one head = one sink;
/// walls are one sink per head — no canvas spanning, brief §9).
///
/// Not `Clone`: the optional [`PresentationPlan`] carries a boxed
/// [`PresentationClock`](super::present::PresentationClock) trait object, which
/// is single-ownership by construction (each sink reads its own clock).
#[derive(Debug)]
pub struct DisplaySinkConfig {
    /// The owning output's stable id (diagnostics/telemetry labels).
    pub output_id: String,
    /// Which connector to drive.
    pub connector: ConnectorSelector,
    /// The mode request (auto / exact override).
    pub mode: ModeRequest,
    /// The CVT-RB forced mode for an EDID-less chain, if configured.
    pub forced_mode: Option<ForcedMode>,
    /// The engine output cadence, for exact-rational refresh matching.
    pub engine_cadence: Option<Rational>,
    /// The bounded event-wait used by the flip loop. Also bounds how quickly
    /// an idle pipe notices a new mailbox frame; a few milliseconds is right.
    pub poll_interval: Duration,
    /// DEV-C2 — the node presentation plan: the outbound presentation epoch,
    /// the fixed receiver-side link offset, and the monotonic/wall clock pair
    /// the pull-side frame chooser runs against. `None` ⇒ the DEV-B1
    /// undisciplined latest-wins loop (a non-node display output).
    pub presentation: Option<PresentationPlan>,
}

/// Wait-free flip-loop telemetry counters (ADR-0044: flip timestamps and
/// conflation are exported from day one; DEV-C2 adds the presentation-discipline
/// counters and the flip-timestamp skew telemetry).
#[derive(Debug, Default)]
pub struct DisplayStats {
    /// Successful nonblocking commits.
    commits: AtomicU64,
    /// Page-flip completions observed.
    flips: AtomicU64,
    /// Commits answered `EBUSY` (mailbox conflation events).
    busy_conflations: AtomicU64,
    /// Device-level submit failures (held last-good instead).
    submit_errors: AtomicU64,
    /// Nanoseconds of the most recent kernel flip timestamp.
    last_flip_ns: AtomicU64,
    /// Hotplug-triggered connector re-probes performed (DEV-B5).
    reprobes: AtomicU64,
    /// Re-light modesets applied after a disconnect→reconnect (DEV-B5).
    relights: AtomicU64,
    /// DEV-C2: disciplined presents — a frame chosen against the predicted
    /// vblank (epoch + flip anchor present).
    presented: AtomicU64,
    /// DEV-C2: undisciplined latest-wins presents — light-up before a flip
    /// anchor exists, or no epoch published (the output never waits for timing).
    undisciplined_presents: AtomicU64,
    /// DEV-C2: queued frames dropped as late (drop-if-late: every frame older
    /// than the one chosen for a vblank, plus pull-side queue overflows).
    late_skips: AtomicU64,
    /// DEV-C2: the most recent flip-timestamp skew (kernel flip ts − the
    /// committed frame's scheduled monotonic instant), in ns. Stored as the
    /// `i64` bit pattern so a negative skew (flip earlier than scheduled) round-
    /// trips exactly through the atomic.
    last_flip_skew_ns: AtomicI64,
    /// DEV-C2: the largest absolute flip-timestamp skew observed (ns) — the
    /// presentation-discipline drift high-watermark.
    max_flip_skew_abs_ns: AtomicI64,
}

/// One coherent read of the sink counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatsSnapshot {
    /// Successful nonblocking commits.
    pub commits: u64,
    /// Page-flip completions observed.
    pub flips: u64,
    /// Commits answered `EBUSY` (conflation events).
    pub busy_conflations: u64,
    /// Device-level submit failures.
    pub submit_errors: u64,
    /// The most recent kernel flip timestamp, in nanoseconds.
    pub last_flip_ns: u64,
    /// Hotplug-triggered connector re-probes performed.
    pub reprobes: u64,
    /// Re-light modesets applied after a disconnect→reconnect.
    pub relights: u64,
    /// DEV-C2: disciplined presents (chosen against the predicted vblank).
    pub presented: u64,
    /// DEV-C2: undisciplined latest-wins presents (no epoch / no flip anchor).
    pub undisciplined_presents: u64,
    /// DEV-C2: queued frames dropped as late (drop-if-late + queue overflow).
    pub late_skips: u64,
    /// DEV-C2: the most recent flip-timestamp skew (flip ts − scheduled), ns.
    pub last_flip_skew_ns: i64,
    /// DEV-C2: the largest absolute flip-timestamp skew observed, ns.
    pub max_flip_skew_abs_ns: i64,
}

impl DisplayStats {
    /// Snapshot the counters (relaxed reads; telemetry only).
    #[must_use]
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            commits: self.commits.load(Ordering::Relaxed),
            flips: self.flips.load(Ordering::Relaxed),
            busy_conflations: self.busy_conflations.load(Ordering::Relaxed),
            submit_errors: self.submit_errors.load(Ordering::Relaxed),
            last_flip_ns: self.last_flip_ns.load(Ordering::Relaxed),
            reprobes: self.reprobes.load(Ordering::Relaxed),
            relights: self.relights.load(Ordering::Relaxed),
            presented: self.presented.load(Ordering::Relaxed),
            undisciplined_presents: self.undisciplined_presents.load(Ordering::Relaxed),
            late_skips: self.late_skips.load(Ordering::Relaxed),
            last_flip_skew_ns: self.last_flip_skew_ns.load(Ordering::Relaxed),
            max_flip_skew_abs_ns: self.max_flip_skew_abs_ns.load(Ordering::Relaxed),
        }
    }
}

/// The display sink entry point. See [`DisplaySink::start`].
#[derive(Debug)]
pub struct DisplaySink;

/// The running sink: owns the flip thread. Dropping the handle (or calling
/// [`DisplaySinkHandle::stop`]) stops and joins the thread — the loop notices
/// the flag within one bounded `poll_interval` wait.
#[derive(Debug)]
pub struct DisplaySinkHandle {
    head: HeadSetup,
    stats: Arc<DisplayStats>,
    stop: Arc<AtomicBool>,
    reprobe: ReprobeFlag,
    thread: Option<JoinHandle<()>>,
}

impl DisplaySinkHandle {
    /// The head (connector + committed timing) this sink drives.
    #[must_use]
    pub fn head(&self) -> &HeadSetup {
        &self.head
    }

    /// The sink's telemetry counters.
    #[must_use]
    pub fn stats(&self) -> Arc<DisplayStats> {
        Arc::clone(&self.stats)
    }

    /// The sink's hotplug re-probe request flag (DEV-B5): the hotplug
    /// monitor (or any caller) requests; the flip-loop thread — the device's
    /// owner — performs the probe between flips, and re-validates +
    /// re-lights the head after a disconnect→reconnect.
    #[must_use]
    pub fn reprobe_flag(&self) -> ReprobeFlag {
        self.reprobe.clone()
    }

    /// Stop the flip loop and join the thread (bounded by the loop's
    /// `poll_interval`-sized waits).
    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // A panicked sink thread must never propagate into the engine's
            // teardown path; the join error is logged and dropped.
            if thread.join().is_err() {
                tracing::error!("display sink thread panicked during the run");
            }
        }
    }
}

impl Drop for DisplaySinkHandle {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

impl DisplaySink {
    /// Start one display sink over `backend`:
    ///
    /// 1. probe connectors and resolve [`DisplaySinkConfig::connector`];
    /// 2. select the timing (EDID preferred / exact-rational cadence match /
    ///    CVT-RB forced fallback — [`select_mode`]);
    /// 3. `TEST_ONLY`-validate, then apply the **one** `ALLOW_MODESET`
    ///    commit (both on this thread — startup is off the frame path by
    ///    construction);
    /// 4. spawn the dedicated flip-loop thread, which owns the device from
    ///    here on.
    ///
    /// Returns the running handle plus the wait-free [`FramePublisher`] the
    /// engine publishes each tick's canvas into.
    ///
    /// # Errors
    ///
    /// [`DisplayError`] when the connector cannot be resolved, no usable mode
    /// exists, validation fails, or the modeset fails. A startup failure is a
    /// run-configuration error; runtime failures after startup never
    /// propagate (the sink holds last-good and keeps trying).
    pub fn start<F, B>(
        mut backend: B,
        config: DisplaySinkConfig,
    ) -> Result<(DisplaySinkHandle, FramePublisher<F>), DisplayError>
    where
        F: DisplayCanvas + Send + Sync + 'static,
        B: KmsBackend + 'static,
    {
        // Consume the config wholesale: every field is either moved into the
        // sink thread or used during startup.
        let DisplaySinkConfig {
            output_id,
            connector,
            mode,
            forced_mode,
            engine_cadence,
            poll_interval,
            presentation,
        } = config;
        let connectors = backend.probe_connectors()?;
        let desc = match &connector {
            ConnectorSelector::Auto => {
                connectors.iter().find(|c| c.connected).ok_or_else(|| {
                    DisplayError::NoneConnected {
                        probed: connectors.iter().map(|c| c.name.clone()).collect(),
                    }
                })?
            }
            ConnectorSelector::Name(name) => {
                let found = connectors.iter().find(|c| &c.name == name).ok_or_else(|| {
                    DisplayError::ConnectorNotFound {
                        requested: name.clone(),
                        available: connectors.iter().map(|c| c.name.clone()).collect(),
                    }
                })?;
                if !found.connected {
                    return Err(DisplayError::NotConnected { name: name.clone() });
                }
                found
            }
        };
        let selected = select_mode(&desc.modes, &mode, forced_mode.as_ref(), engine_cadence)?;
        let from_edid = matches!(selected, SelectedMode::Edid(_));
        let setup = HeadSetup {
            connector: desc.name.clone(),
            mode: selected.mode().clone(),
            from_edid,
        };
        // TEST_ONLY first: the exact plane/format/mode combination is proven
        // before any hardware state changes (brief §1 step 5).
        backend.validate_setup(&setup)?;
        // The ONE ALLOW_MODESET commit — startup only, never the frame path.
        backend.apply_modeset(&setup)?;
        tracing::info!(
            output = %output_id,
            connector = %setup.connector,
            mode = %setup.mode.describe(),
            from_edid,
            "display head lit"
        );

        // DEV-C2: the vblank predictor is anchored on the COMMITTED scanout
        // mode's exact-rational refresh (the device's pixel clock / raster
        // totals), not the engine cadence — the predictor lives in the
        // display's vblank domain.
        let refresh = setup.mode.refresh();
        let (publisher, reader) = frame_mailbox::<F>();
        let stats = Arc::new(DisplayStats::default());
        let stop = Arc::new(AtomicBool::new(false));
        let reprobe = ReprobeFlag::new();
        let thread = {
            let stats = Arc::clone(&stats);
            let stop = Arc::clone(&stop);
            let reprobe = reprobe.clone();
            let head = setup.clone();
            let poll = poll_interval;
            std::thread::Builder::new()
                .name(format!("display-{}", setup.connector))
                .spawn(move || {
                    flip_loop(
                        backend,
                        &reader,
                        &stop,
                        &stats,
                        poll,
                        &output_id,
                        &head,
                        &reprobe,
                        presentation,
                        refresh,
                    );
                })
                .map_err(|e| DisplayError::Device(format!("spawning the sink thread: {e}")))?
        };
        Ok((
            DisplaySinkHandle {
                head: setup,
                stats,
                stop,
                reprobe,
                thread: Some(thread),
            },
            publisher,
        ))
    }
}

/// The dedicated sink-thread loop (ADR-0044 §1): bounded event wait → drain
/// flip completions → when idle and a **newer** mailbox frame exists, write +
/// `atomic_commit(NONBLOCK | PAGE_FLIP_EVENT)`. `EBUSY` = conflation; device
/// errors are counted and the last-good framebuffer stays on glass; nothing
/// here can reach back into the engine.
///
/// Hotplug (DEV-B5): between flips the loop also consumes its [`ReprobeFlag`]
/// — when requested it probes the connectors (the userspace probe IS the
/// `force_probe`), and on a disconnect→reconnect transition re-validates
/// (`TEST_ONLY`) and re-applies the committed modeset to re-light the head
/// (DP link retraining / HDMI re-handshake). Probe and re-light failures are
/// counted and logged; the loop never exits over them.
#[allow(clippy::too_many_arguments)]
// reason: the loop is the move-target of the one sink thread; every argument
// is one owned/shared piece of the sink's state, and bundling them into a
// struct would only relocate the same names.
fn flip_loop<F, B>(
    mut backend: B,
    reader: &FrameReader<F>,
    stop: &AtomicBool,
    stats: &DisplayStats,
    poll_interval: Duration,
    output_id: &str,
    head: &HeadSetup,
    reprobe: &ReprobeFlag,
    presentation: Option<PresentationPlan>,
    refresh: Rational,
) where
    F: DisplayCanvas + Send + Sync,
    B: KmsBackend,
{
    let mut driver = FlipDriver::new();
    // Startup proved the connector connected (a disconnected head fails
    // `DisplaySink::start`); reprobe transitions are tracked against that.
    let mut connected = true;
    // DEV-C2 pull-side presentation state (only used when a plan is present):
    // the bounded present queue, the flip-anchored vblank predictor, the
    // sequence last drained from the mailbox (so the queue takes each frame
    // once), and the scheduled monotonic instant of the in-flight committed
    // frame (for the flip-timestamp skew telemetry).
    let mut present = presentation.map(|plan| PresentState::<F> {
        plan,
        queue: PresentQueue::new(),
        predictor: VblankPredictor::new(refresh),
        drained_seq: 0,
        in_flight_scheduled_mono_ns: None,
    });
    while !stop.load(Ordering::Acquire) {
        if reprobe.take() {
            handle_reprobe(&mut backend, head, &mut connected, stats, output_id);
        }
        match backend.wait_events(poll_interval) {
            Ok(events) => {
                for event in events {
                    driver.on_flip_complete();
                    stats.flips.fetch_add(1, Ordering::Relaxed);
                    let flip_ns = i64::try_from(event.timestamp.as_nanos()).unwrap_or(i64::MAX);
                    stats.last_flip_ns.store(
                        u64::try_from(flip_ns).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                    if let Some(pres) = present.as_mut() {
                        // Re-anchor the vblank grid on the measured flip, and
                        // export the flip-timestamp skew of the frame this flip
                        // completes (flip ts − its scheduled monotonic instant).
                        pres.predictor.on_flip(flip_ns);
                        if let Some(scheduled) = pres.in_flight_scheduled_mono_ns.take() {
                            record_flip_skew(stats, flip_ns, scheduled);
                        }
                    }
                }
            }
            Err(e) => {
                // An event-channel failure is survivable: log, back off one
                // interval, keep the last framebuffer on glass.
                tracing::warn!(output = %output_id, error = %e, "display event wait failed");
                std::thread::sleep(poll_interval);
            }
        }
        match present.as_mut() {
            Some(pres) => {
                disciplined_step(&mut backend, reader, &mut driver, stats, output_id, pres);
            }
            None => {
                undisciplined_step(&mut backend, reader, &mut driver, stats, output_id);
            }
        }
    }
}

/// The DEV-C2 pull-side presentation state carried across loop iterations
/// (present only on a node sink with a [`PresentationPlan`]). The present queue
/// holds [`MailboxFrame`] clones — cheap `Arc` bumps, so the bounded queue
/// keeps 2–3 frames alive without ever copying a canvas.
struct PresentState<F> {
    plan: PresentationPlan,
    queue: PresentQueue<MailboxFrame<F>>,
    predictor: VblankPredictor,
    drained_seq: u64,
    in_flight_scheduled_mono_ns: Option<i64>,
}

/// The DEV-B1 undisciplined loop body: commit the latest mailbox frame when the
/// pipe is idle and the frame is newer than what was last committed. `EBUSY` is
/// conflation; device errors hold last-good. Used for non-node display outputs.
fn undisciplined_step<F, B>(
    backend: &mut B,
    reader: &FrameReader<F>,
    driver: &mut FlipDriver,
    stats: &DisplayStats,
    output_id: &str,
) where
    F: DisplayCanvas + Send + Sync,
    B: KmsBackend,
{
    let Some((frame, seq)) = reader.latest() else {
        return;
    };
    if !driver.wants_commit(seq) {
        return;
    }
    match backend.submit_frame(&*frame) {
        Ok(()) => {
            driver.on_commit_submitted(seq);
            stats.commits.fetch_add(1, Ordering::Relaxed);
        }
        Err(SubmitError::Busy) => {
            driver.on_commit_busy();
            stats.busy_conflations.fetch_add(1, Ordering::Relaxed);
        }
        Err(SubmitError::Device(e)) => {
            stats.submit_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                output = %output_id,
                error = %e,
                "display frame commit failed; holding the last framebuffer"
            );
        }
    }
}

/// The DEV-C2 disciplined loop body: drain the latest mailbox frame into the
/// bounded present queue, then (if the pipe is idle) choose the queued frame
/// nearest the predicted vblank — `wall_at(pts) + link_offset` closest to the
/// predicted next vblank, repeat-if-early, drop-if-late — and commit it.
///
/// Falls back to undisciplined latest-wins (presenting the newest queued frame)
/// whenever the epoch is unpublished or the predictor has no flip anchor yet:
/// the output never waits for timing. A lost controller feed only stops epoch
/// updates — the node keeps the last epoch and free-runs (display-out §8).
fn disciplined_step<F, B>(
    backend: &mut B,
    reader: &FrameReader<F>,
    driver: &mut FlipDriver,
    stats: &DisplayStats,
    output_id: &str,
    pres: &mut PresentState<F>,
) where
    F: DisplayCanvas + Send + Sync,
    B: KmsBackend,
{
    // Drain the latest mailbox frame into the bounded present queue exactly
    // once (newest-wins; the engine-side publish stays wait-free — this is the
    // pull side of the seam). A queue overflow is a late skip (drop-oldest).
    if let Some((frame, seq)) = reader.latest() {
        if seq > pres.drained_seq {
            let pts_ns = frame.pts_ns();
            if pres.queue.push(frame, seq, pts_ns) {
                stats.late_skips.fetch_add(1, Ordering::Relaxed);
            }
            pres.drained_seq = seq;
        }
    }
    // A commit is in flight: the kernel allows at most one per CRTC, so no
    // decision can land until the pending flip drains (KMS repeats the glass).
    if driver.in_flight() || pres.queue.is_empty() {
        return;
    }
    let (mono_now_ns, wall_now_ns) = pres.plan.clock.now_pair();
    let wall_minus_mono = wall_now_ns.saturating_sub(mono_now_ns);
    // Disciplined choice needs BOTH a published epoch and a flip anchor. Either
    // missing ⇒ honest undisciplined latest-wins (present the newest queued
    // frame). The output never waits for timing.
    let (choice, disciplined) = match (
        pres.plan.epoch.get(),
        pres.predictor.predicted_next_ns(mono_now_ns),
    ) {
        (Some(epoch), Some(vblank_mono_ns)) => {
            // The deadlines are in the epoch's WALL domain; the predicted vblank
            // is MONOTONIC (the KMS flip-timestamp domain). Bridge the vblank
            // into the wall domain with the just-sampled `wall − mono` offset so
            // the comparison is apples-to-apples.
            let deadlines = pres.queue.deadlines(epoch, pres.plan.link_offset_ns);
            let vblank_wall_ns = vblank_mono_ns.saturating_add(wall_minus_mono);
            (
                choose_frame(&deadlines, vblank_wall_ns, pres.predictor.period_ns()),
                true,
            )
        }
        // Latest-wins: the newest queued frame (the back of the queue). The
        // present-queue is non-empty here (checked above).
        _ => (
            FrameChoice::Present {
                index: pres.queue.len().saturating_sub(1),
            },
            false,
        ),
    };
    match choice {
        FrameChoice::Idle | FrameChoice::RepeatEarly => {
            // Nothing to present this vblank, or the nearest frame belongs to
            // the next vblank: KMS repeats the current framebuffer for free.
        }
        FrameChoice::Present { index } => {
            commit_chosen(
                backend,
                driver,
                stats,
                output_id,
                pres,
                index,
                wall_minus_mono,
                disciplined,
            );
        }
    }
}

/// Commit the queued frame at `index` (already chosen): submit it, and on
/// success consume the queue through it (every earlier entry is a late skip),
/// record its scheduled monotonic instant for the flip-skew telemetry, and bump
/// the present counters (disciplined vs undisciplined). `EBUSY` keeps the chosen
/// frame queued (the retry candidate after the pending flip drains); a device
/// error holds last-good.
#[allow(clippy::too_many_arguments)]
// reason: each argument is one distinct piece of the commit's context (the
// device, the flip driver, the counters, the chosen index, the clock offset,
// and the disciplined/undisciplined classification); a wrapper struct would
// only relocate the same names.
fn commit_chosen<F, B>(
    backend: &mut B,
    driver: &mut FlipDriver,
    stats: &DisplayStats,
    output_id: &str,
    pres: &mut PresentState<F>,
    index: usize,
    wall_minus_mono: i64,
    disciplined: bool,
) where
    F: DisplayCanvas + Send + Sync,
    B: KmsBackend,
{
    // Clone the chosen frame out of the queue (a cheap `Arc` bump) so the queue
    // borrow is released before the mutable `pop_through` below.
    let Some((frame, seq, pts_ns)) = pres.queue.entry(index).map(|(f, s, p)| (f.clone(), s, p))
    else {
        return;
    };
    match backend.submit_frame(&*frame) {
        Ok(()) => {
            driver.on_commit_submitted(seq);
            stats.commits.fetch_add(1, Ordering::Relaxed);
            if disciplined {
                stats.presented.fetch_add(1, Ordering::Relaxed);
            } else {
                stats.undisciplined_presents.fetch_add(1, Ordering::Relaxed);
            }
            // The chosen frame's scheduled MONOTONIC instant — its wall deadline
            // mapped back through the just-sampled `wall − mono` offset — so the
            // flip-completion handler can export the skew against the kernel's
            // flip timestamp (which is monotonic). Disciplined deadlines use the
            // epoch; without one (undisciplined) the scheduled instant is the
            // frame's own pts + link offset in the mono domain, the honest
            // fallback (skew telemetry stays meaningful pre-epoch).
            let deadline_wall = match pres.plan.epoch.get() {
                Some(epoch) => epoch
                    .wall_at(pts_ns)
                    .saturating_add(pres.plan.link_offset_ns),
                None => pts_ns
                    .saturating_add(wall_minus_mono)
                    .saturating_add(pres.plan.link_offset_ns),
            };
            pres.in_flight_scheduled_mono_ns = Some(deadline_wall.saturating_sub(wall_minus_mono));
            // Consume the queue through the chosen frame: every earlier entry
            // was a late skip (drop-if-late).
            let skips = pres.queue.pop_through(index);
            if skips > 0 {
                stats
                    .late_skips
                    .fetch_add(u64::try_from(skips).unwrap_or(u64::MAX), Ordering::Relaxed);
            }
        }
        Err(SubmitError::Busy) => {
            // EBUSY = an unaccounted-for kernel flip is pending: become
            // in-flight WITHOUT consuming the chosen frame — it is the retry
            // candidate after the pending flip drains.
            driver.on_commit_busy();
            stats.busy_conflations.fetch_add(1, Ordering::Relaxed);
        }
        Err(SubmitError::Device(e)) => {
            stats.submit_errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                output = %output_id,
                error = %e,
                "display frame commit failed; holding the last framebuffer"
            );
        }
    }
}

/// Record one flip-timestamp skew sample (DEV-C2): `flip_ns − scheduled_ns`
/// (kernel flip timestamp minus the committed frame's scheduled monotonic
/// instant), and update the absolute high-watermark. Both are monotonic-domain
/// ns, so the subtraction is exact.
fn record_flip_skew(stats: &DisplayStats, flip_ns: i64, scheduled_ns: i64) {
    let skew = flip_ns.saturating_sub(scheduled_ns);
    stats.last_flip_skew_ns.store(skew, Ordering::Relaxed);
    let abs = skew.saturating_abs();
    // Monotonic max via a relaxed CAS loop (telemetry only — no ordering needs).
    let mut cur = stats.max_flip_skew_abs_ns.load(Ordering::Relaxed);
    while abs > cur {
        match stats.max_flip_skew_abs_ns.compare_exchange_weak(
            cur,
            abs,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => cur = observed,
        }
    }
}

/// One hotplug-triggered re-probe on the flip-loop thread (DEV-B5): probe the
/// connectors, track the bound connector's connected state, and on a
/// disconnect→reconnect transition re-validate (`TEST_ONLY`) then re-apply
/// the committed modeset to re-light the head. Every failure is logged and
/// counted; none ends the loop (the engine never participates — invariants
/// #1 + #10).
fn handle_reprobe<B: KmsBackend>(
    backend: &mut B,
    head: &HeadSetup,
    connected: &mut bool,
    stats: &DisplayStats,
    output_id: &str,
) {
    stats.reprobes.fetch_add(1, Ordering::Relaxed);
    let connectors = match backend.probe_connectors() {
        Ok(connectors) => connectors,
        Err(e) => {
            tracing::warn!(
                output = %output_id,
                connector = %head.connector,
                error = %e,
                "hotplug re-probe failed; keeping the current state"
            );
            return;
        }
    };
    let now_connected = connectors
        .iter()
        .find(|c| c.name == head.connector)
        .is_some_and(|c| c.connected);
    match (*connected, now_connected) {
        (true, false) => {
            *connected = false;
            tracing::warn!(
                output = %output_id,
                connector = %head.connector,
                "display disconnected; holding the last framebuffer (KMS keeps scanning it \
                 out; the head re-lights on reconnect)"
            );
        }
        (false, true) => {
            // Re-light with the COMMITTED setup: TEST_ONLY first (the
            // attached sink may have changed and might reject the timing),
            // then the one re-light modeset. A failure leaves the head dark
            // until the next hotplug event retries.
            if let Err(e) = backend.validate_setup(head) {
                tracing::warn!(
                    output = %output_id,
                    connector = %head.connector,
                    error = %e,
                    "reconnected display rejected the committed mode (TEST_ONLY); leaving \
                     the head dark — reconfigure the output for the new sink"
                );
                return;
            }
            match backend.apply_modeset(head) {
                Ok(()) => {
                    *connected = true;
                    stats.relights.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(
                        output = %output_id,
                        connector = %head.connector,
                        mode = %head.mode.describe(),
                        "display reconnected: head re-lit with the committed mode"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        output = %output_id,
                        connector = %head.connector,
                        error = %e,
                        "re-light modeset failed; the next hotplug event retries"
                    );
                }
            }
        }
        // No transition: nothing to do (the probe itself refreshed the
        // kernel's connector state, which is the point of force_probe
        // polling).
        (true, true) | (false, false) => {}
    }
}
