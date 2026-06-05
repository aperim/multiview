//! The `multiview run` subcommand and its **headless software engine**.
//!
//! [`HeadlessEngine`] wires the protected output core from a validated
//! [`MultiviewConfig`]:
//!
//! * a fixed-cadence [`OutputClock`] at the canvas cadence (exact rational);
//! * one [`TileStore<Nv12Image>`] per declared source, holding a synthetic
//!   test-pattern frame (built-in `test` sources publish into these stores);
//! * a CPU reference [`CompositorDrive`] over the solved
//!   [`multiview_core::layout::Layout`]; and
//! * the engine's outbound [`EnginePublisher`] (invariant #10 isolation).
//!
//! It then drives exactly one composited frame per tick for a bounded number of
//! ticks (or until Ctrl-C in the binary), and reports the outcome. This is a
//! GPU-free, `FFmpeg`-free **software end-to-end smoke of invariant #1**: the
//! output emits one valid frame per tick, on cadence, forever, independent of
//! input health.
//!
//! The driver is the engine's own [`EngineRuntime`], parameterized by an
//! injected [`TimeSource`] + [`Pacer`] so the same code runs deterministically
//! in tests (manual time + cooperative pacer, no real sleeps) and in production
//! (monotonic time + realtime pacer).
use std::collections::HashMap;
use std::sync::Arc;

use multiview_compositor::blend::LinearRgba;
use multiview_compositor::pipeline::{CanvasColor, Nv12Image};
use multiview_config::MultiviewConfig;
use multiview_core::layout::Layout;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::{
    CompositorDrive, EnginePublisher, EngineRuntime, ManualTimeSource, MonotonicTimeSource,
    OutputClock, Pacer, RealtimePacer, RunStop, StopSignal, TimeSource,
};
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

/// The per-subscriber drop-oldest depth of the engine's outbound event stream.
/// Headless has no consumers, but the publisher still needs a positive ring.
const EVENT_CAPACITY: usize = 64;

/// The state snapshot the headless engine publishes each tick (invariant #10):
/// the tick index and its presentation timestamp. Best-effort; no consumer can
/// back-pressure its publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeadlessState {
    /// The tick index this snapshot was produced for.
    pub tick: u64,
    /// The presentation timestamp of the tick (`out_pts = f(tick)`).
    pub pts: MediaTime,
}

/// A summary of a headless run: how many ticks/frames were produced, the
/// cadence, the canvas geometry, the PTS span, and whether the output ever
/// faltered (it must not).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RunReport {
    /// Ticks emitted by the output clock.
    pub ticks: u64,
    /// Frames composited and published (must equal [`RunReport::ticks`]).
    pub frames: u64,
    /// The fixed output cadence (exact rational, never a float fps).
    pub cadence: Rational,
    /// Output canvas width in pixels.
    pub canvas_width: u32,
    /// Output canvas height in pixels.
    pub canvas_height: u32,
    /// The PTS of the first frame, if any frame was produced.
    pub first_pts: Option<MediaTime>,
    /// The PTS of the last frame, if any frame was produced.
    pub last_pts: Option<MediaTime>,
    /// Whether the output ever faltered: `true` if frames != ticks, or a
    /// frame's PTS failed to advance monotonically. **Must be `false`.**
    pub faltered: bool,
}

impl RunReport {
    /// Render the report as the multi-line text the binary prints.
    #[must_use]
    pub fn render(&self) -> String {
        let cadence = self.cadence;
        let span = match (self.first_pts, self.last_pts) {
            (Some(first), Some(last)) => {
                format!("pts {}..={} ns", first.as_nanos(), last.as_nanos())
            }
            _ => "no frames".to_owned(),
        };
        let verdict = if self.faltered {
            "FALTERED"
        } else {
            "never faltered"
        };
        format!(
            "headless run: {} frame(s) for {} tick(s) at {}/{} fps on {}x{}; {}; output {}",
            self.frames,
            self.ticks,
            cadence.num,
            cadence.den,
            self.canvas_width,
            self.canvas_height,
            span,
            verdict,
        )
    }
}

/// Errors that can occur building or running the headless engine.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RunError {
    /// The configuration failed validation before any engine was built.
    #[error("invalid configuration: {0}")]
    Config(#[from] multiview_config::ConfigError),
    /// The output clock rejected the canvas cadence.
    #[error("output clock: {0}")]
    Clock(String),
    /// The compositor drive or canvas was rejected by the engine.
    #[error("engine: {0}")]
    Engine(String),
    /// A synthetic test-pattern frame could not be built.
    #[error("test pattern: {0}")]
    Pattern(String),
}

/// A built, ready-to-run headless software engine.
///
/// Construct one with [`HeadlessEngine::build`] from a validated config, then
/// drive it with [`HeadlessEngine::run_for`] (deterministic, injected time) or
/// [`HeadlessEngine::run_for_realtime`] / [`HeadlessEngine::run_until_stopped`]
/// (production wall-clock pacing).
///
/// The engine is consumed-shaped: each `run_*` method takes `&mut self` and is
/// intended to be driven once; it rebuilds the compositor drive per run so the
/// stores (and their synthetic frames) are reused intact.
pub struct HeadlessEngine {
    /// The solved layout (canvas + normalized cells), shared into the drive.
    layout: Arc<Layout>,
    /// The fixed output cadence (exact rational).
    cadence: Rational,
    /// Per-source last-good-frame stores, keyed by source id.
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    /// The synthetic test-pattern frame for each source (kept so a run can
    /// (re)publish it into the stores).
    patterns: HashMap<String, Arc<Nv12Image>>,
    /// The fixed canvas color (ADR-C001 SDR BT.709 limited by default).
    canvas_color: CanvasColor,
    /// The "no signal" slate composited for tiles with no usable frame.
    nosignal_card: Nv12Image,
    /// The canvas background shown where no tile covers.
    background: LinearRgba,
    /// Whether to publish synthetic test-pattern frames into the stores at the
    /// start of a run (default `true`). Set `false` to prove the output is
    /// independent of input health (every tile shows the slate).
    publish_test_frames: bool,
}

impl HeadlessEngine {
    /// Build a headless engine from an already-validated configuration.
    ///
    /// Solves the layout, creates one [`TileStore`] per source (with
    /// [`NoSignalPolicy::HoldForever`] so a once-published synthetic frame stays
    /// available across the whole bounded run), and builds a distinctly-colored
    /// NV12 test pattern for each source.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Config`] if the layout cannot be solved (the document
    /// should be validated first), or [`RunError::Pattern`] if a synthetic frame
    /// cannot be constructed for the canvas geometry.
    pub fn build(config: &MultiviewConfig) -> Result<Self, RunError> {
        let layout = config.solve_layout()?;
        let cadence = config.canvas.fps.rational();
        let canvas_color = CanvasColor::default();
        let tag = canvas_color.output_tag();

        // One store per declared source. HoldForever keeps the synthetic frame
        // available for the whole bounded run regardless of how far the manual
        // clock advances; the *state* still rides the LIVE/STALE ladder.
        let mut stores: HashMap<String, Arc<TileStore<Nv12Image>>> =
            HashMap::with_capacity(config.sources.len());
        let mut patterns: HashMap<String, Arc<Nv12Image>> =
            HashMap::with_capacity(config.sources.len());

        for (index, source) in config.sources.iter().enumerate() {
            let store = Arc::new(TileStore::new(
                source.id.clone(),
                TileThresholds::default(),
                NoSignalPolicy::HoldForever,
            ));
            stores.insert(source.id.clone(), Arc::clone(&store));

            let pattern = test_pattern(config.canvas.width, config.canvas.height, index, tag)
                .map_err(|e| RunError::Pattern(e.to_string()))?;
            patterns.insert(source.id.clone(), Arc::new(pattern));
        }

        // The slate card spans the whole canvas; a tile with no usable frame
        // contributes it (mid-gray, tagged like the canvas).
        let nosignal_card =
            Nv12Image::solid(config.canvas.width, config.canvas.height, 16, 128, 128, tag)
                .map_err(|e| RunError::Pattern(e.to_string()))?;

        Ok(Self {
            layout: Arc::new(layout),
            cadence,
            stores,
            patterns,
            canvas_color,
            nosignal_card,
            background: LinearRgba::opaque(0.02, 0.02, 0.05),
            publish_test_frames: true,
        })
    }

    /// The fixed output cadence (exact rational).
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// The number of per-source frame stores wired into this engine.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.stores.len()
    }

    /// Control whether synthetic test-pattern frames are published into the
    /// stores at the start of a run.
    ///
    /// Default `true`: each `test` source's tile shows its pattern (LIVE). Set
    /// `false` to leave every store empty, proving the output produces a valid
    /// slate frame per tick even with no inputs (invariant #1 + #2).
    pub fn set_publish_test_frames(&mut self, publish: bool) {
        self.publish_test_frames = publish;
    }

    /// Drive the engine for exactly `max_ticks` ticks under an injected,
    /// jumpable [`TimeSource`] + [`Pacer`], publishing synthetic frames first.
    ///
    /// Deterministic and **sleep-free** when wired with a [`ManualTimeSource`] +
    /// [`multiview_engine::CooperativePacer`]: before driving, the manual clock is jumped past the
    /// last tick's deadline so the pacer never gates, and the loop emits exactly
    /// one frame per tick as fast as the executor cooperatively yields. Produces
    /// and publishes one frame per tick via the engine's own [`EngineRuntime`];
    /// the per-tick state/event publish goes out through the wait-free isolation
    /// channels and cannot be back-pressured.
    ///
    /// # Errors
    ///
    /// Returns [`RunError::Clock`] if the cadence is rejected, or
    /// [`RunError::Engine`] if the compositor drive/canvas is rejected.
    pub async fn run_for<TS, P>(
        &mut self,
        time_source: Arc<TS>,
        pacer: P,
        max_ticks: u64,
    ) -> Result<RunReport, RunError>
    where
        TS: Advanceable + 'static,
        P: Pacer,
    {
        self.prime_stores(time_source.as_ref());
        let ts: Arc<dyn TimeSource> = time_source.clone();
        let mut runtime = self.build_runtime(ts, pacer)?;
        // Jump the (jumpable) clock past the deadline of the final tick so the
        // cooperative pacer releases every tick of this bounded run without any
        // real sleep. The last tick emitted has index `max_ticks - 1`; covering
        // its deadline (`seed + pts_at(max_ticks - 1)`) plus a tick of headroom
        // releases the whole run. Computing against `max_ticks` is a safe
        // over-estimate.
        let headroom = runtime
            .seed_nanos()
            .saturating_add(pts_at(self.cadence, max_ticks).as_nanos());
        time_source.advance_to(headroom);
        let publisher: EnginePublisher<HeadlessState, HeadlessState> =
            EnginePublisher::new(EVENT_CAPACITY);
        let stop = StopSignal::new();
        self.drive(&mut runtime, &publisher, &stop, Some(max_ticks))
            .await
    }

    /// Drive the engine for `max_ticks` ticks under the production realtime
    /// pacer (monotonic time, real `sleep`s). Used by the binary's `--ticks`
    /// path and by realtime soak tests; paces to the wall clock.
    ///
    /// # Errors
    ///
    /// See [`HeadlessEngine::run_for`].
    pub async fn run_for_realtime(&mut self, max_ticks: u64) -> Result<RunReport, RunError> {
        let time = Arc::new(MonotonicTimeSource::new());
        let ts: Arc<dyn TimeSource> = time;
        self.prime_stores(ts.as_ref());
        let mut runtime = self.build_runtime(ts, RealtimePacer)?;
        let publisher: EnginePublisher<HeadlessState, HeadlessState> =
            EnginePublisher::new(EVENT_CAPACITY);
        let stop = StopSignal::new();
        self.drive(&mut runtime, &publisher, &stop, Some(max_ticks))
            .await
    }

    /// Drive the engine **forever** under the production realtime pacer until
    /// `stop` is raised (the binary wires this to Ctrl-C).
    ///
    /// # Errors
    ///
    /// See [`HeadlessEngine::run_for`].
    pub async fn run_until_stopped(&mut self, stop: &StopSignal) -> Result<RunReport, RunError> {
        let time = Arc::new(MonotonicTimeSource::new());
        let ts: Arc<dyn TimeSource> = time;
        self.prime_stores(ts.as_ref());
        let mut runtime = self.build_runtime(ts, RealtimePacer)?;
        let publisher: EnginePublisher<HeadlessState, HeadlessState> =
            EnginePublisher::new(EVENT_CAPACITY);
        self.drive(&mut runtime, &publisher, stop, None).await
    }

    /// Publish each source's synthetic frame into its store at the current
    /// time-source instant (so the tile reads LIVE), unless publishing is
    /// disabled.
    fn prime_stores(&self, time_source: &dyn TimeSource) {
        if !self.publish_test_frames {
            return;
        }
        let at = MediaTime::from_nanos(time_source.now_nanos());
        for (id, pattern) in &self.patterns {
            if let Some(store) = self.stores.get(id) {
                store.publish_arc(Arc::clone(pattern), at);
            }
        }
    }

    /// Build a fresh [`EngineRuntime`] over a fresh [`CompositorDrive`] sharing
    /// this engine's stores.
    fn build_runtime<P: Pacer>(
        &self,
        time_source: Arc<dyn TimeSource>,
        pacer: P,
    ) -> Result<EngineRuntime<P>, RunError> {
        let clock = OutputClock::new(self.cadence).map_err(|e| RunError::Clock(e.to_string()))?;
        let drive = CompositorDrive::new(
            Arc::clone(&self.layout),
            self.stores.clone(),
            self.nosignal_card.clone(),
            self.canvas_color,
            self.background,
        )
        .map_err(|e| RunError::Engine(e.to_string()))?;
        Ok(EngineRuntime::new(clock, drive, time_source, pacer))
    }

    /// Run the engine's tick loop and fold the outcome into a [`RunReport`],
    /// verifying the output never faltered (frames == ticks, monotone PTS).
    ///
    /// The projection closures are cheap, panic-free, and run on the hot loop;
    /// they publish a per-tick state snapshot and event through the (non-blocking,
    /// drop-oldest) isolation channels — best-effort, never back-pressuring.
    async fn drive<P: Pacer>(
        &self,
        runtime: &mut EngineRuntime<P>,
        publisher: &EnginePublisher<HeadlessState, HeadlessState>,
        stop: &StopSignal,
        max_ticks: Option<u64>,
    ) -> Result<RunReport, RunError> {
        let state_of = |frame: &multiview_engine::CompositedFrame| HeadlessState {
            tick: frame.tick.index,
            pts: frame.pts(),
        };
        let event_of = |frame: &multiview_engine::CompositedFrame| {
            Some(HeadlessState {
                tick: frame.tick.index,
                pts: frame.pts(),
            })
        };

        let outcome = match max_ticks {
            Some(max) => {
                runtime
                    .run_for(publisher, stop, max, state_of, event_of)
                    .await
            }
            None => runtime.run(publisher, stop, state_of, event_of).await,
        }
        .map_err(|e| RunError::Engine(e.to_string()))?;

        let frames = outcome.ticks;
        let first_pts = (frames > 0).then(|| pts_at(self.cadence, 0));
        let last_pts = frames
            .checked_sub(1)
            .map(|last_index| pts_at(self.cadence, last_index));

        // Falter check: the runtime emits exactly one frame per tick by
        // construction, and PTS is `f(tick)` (strictly increasing for a positive
        // cadence). We re-assert that contract in the report rather than assume
        // it: any deviation (a short loop, a non-advancing PTS) flips `faltered`.
        let monotone = match (first_pts, last_pts) {
            (Some(first), Some(last)) => frames <= 1 || last.as_nanos() > first.as_nanos(),
            _ => true,
        };
        let stopped_cleanly = matches!(outcome.stop, RunStop::Completed | RunStop::Stopped);
        let count_matches = match max_ticks {
            Some(max) => frames == max,
            None => true,
        };
        let faltered = !(monotone && stopped_cleanly && count_matches);

        Ok(RunReport {
            ticks: outcome.ticks,
            frames,
            cadence: self.cadence,
            canvas_width: self.layout.canvas.width,
            canvas_height: self.layout.canvas.height,
            first_pts,
            last_pts,
            faltered,
        })
    }
}

/// A [`TimeSource`] whose position can be set forward, for the deterministic
/// (sleep-free) bounded headless run.
///
/// Implemented for [`ManualTimeSource`] ([`ManualTimeSource::set`] jumps the
/// clock). A real monotonic source cannot be jumped, so the realtime path uses
/// [`HeadlessEngine::run_for_realtime`] (which paces against true elapsed time)
/// instead of [`HeadlessEngine::run_for`].
pub trait Advanceable: TimeSource {
    /// Move the source forward to at least `nanos` (never backwards).
    fn advance_to(&self, nanos: i64);
}

impl Advanceable for ManualTimeSource {
    fn advance_to(&self, nanos: i64) {
        self.set(nanos);
    }
}

/// The PTS of tick `index` at `cadence` (`out_pts = f(tick)`, exact, never
/// float-accumulated).
fn pts_at(cadence: Rational, index: u64) -> MediaTime {
    let tick = i64::try_from(index).unwrap_or(i64::MAX);
    MediaTime::from_tick(tick, cadence)
}

/// Build a distinctly-colored NV12 test pattern for source `index`, tagged like
/// the canvas. Cycles through a small palette of luma/chroma triples so adjacent
/// tiles are visually distinct in the composite.
///
/// # Errors
///
/// Returns the compositor [`multiview_compositor::Error`] if the geometry is
/// rejected (odd/zero dimensions).
fn test_pattern(
    width: u32,
    height: u32,
    index: usize,
    tag: multiview_core::color::ColorInfo,
) -> Result<Nv12Image, multiview_compositor::Error> {
    // A small palette of (Y, Cb, Cr) limited-range code values: gray bars in
    // distinct hues. Index modulo the palette length keeps it total.
    const PALETTE: [(u8, u8, u8); 8] = [
        (180, 90, 240),  // reddish
        (170, 240, 110), // bluish
        (150, 44, 142),  // greenish
        (200, 128, 128), // light gray
        (120, 200, 60),  // teal-ish
        (90, 160, 200),  // amber-ish
        (210, 128, 128), // near white
        (40, 128, 128),  // near black
    ];
    let (y, cb, cr) = PALETTE
        .get(index % PALETTE.len())
        .copied()
        .unwrap_or((128, 128, 128));
    Nv12Image::solid(width, height, y, cb, cr, tag)
}
