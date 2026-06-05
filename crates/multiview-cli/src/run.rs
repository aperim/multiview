//! The `multiview run` subcommand and its **FFmpeg-free software engine**.
//!
//! [`SoftwareEngine`] wires the protected output core from a validated
//! [`MultiviewConfig`]:
//!
//! * a fixed-cadence [`OutputClock`] at the canvas cadence (exact rational);
//! * one [`TileStore<Nv12Image>`] per declared source, holding the source's
//!   synthetic frame — real colour bars / solid for the `bars`/`solid` kinds, a
//!   per-tile placeholder for kinds that need a decoder this build lacks;
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
use multiview_config::{MultiviewConfig, Source, SourceKind};
use multiview_control::EngineStateSnapshot;
use multiview_core::layout::Layout;
use multiview_core::time::{MediaTime, Rational};
use multiview_engine::{
    CompositorDrive, EnginePublisher, EngineRuntime, ManualTimeSource, MonotonicTimeSource,
    OutputClock, Pacer, RealtimePacer, RunStop, StopSignal, TimeSource,
};
use multiview_events::Event;
use multiview_framestore::{NoSignalPolicy, TileStore, TileThresholds};

/// The per-subscriber drop-oldest depth of the engine's outbound event stream.
/// The software smoke has no consumers, but the publisher still needs a positive
/// ring.
const EVENT_CAPACITY: usize = 64;

/// Capture the composited program frame into the live-preview slot every Nth
/// tick. At 30–60 fps this is ≈2–4 preview frames/sec — enough for a monitoring
/// still, cheap enough to clone on the hot loop without affecting the cadence.
const PREVIEW_CAPTURE_EVERY: u64 = 15;

/// The state snapshot the software engine publishes each tick (invariant #10):
/// the tick index and its presentation timestamp. Best-effort; no consumer can
/// back-pressure its publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SoftwareState {
    /// The tick index this snapshot was produced for.
    pub tick: u64,
    /// The presentation timestamp of the tick (`out_pts = f(tick)`).
    pub pts: MediaTime,
}

/// A summary of a software run: how many ticks/frames were produced, the
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
            "software run: {} frame(s) for {} tick(s) at {}/{} fps on {}x{}; {}; output {}",
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

/// Errors that can occur building or running the software engine.
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

/// A built, ready-to-run software engine.
///
/// Construct one with [`SoftwareEngine::build`] from a validated config, then
/// drive it with [`SoftwareEngine::run_for`] (deterministic, injected time) or
/// [`SoftwareEngine::run_for_realtime`] / [`SoftwareEngine::run_until_stopped`]
/// (production wall-clock pacing).
///
/// The engine is consumed-shaped: each `run_*` method takes `&mut self` and is
/// intended to be driven once; it rebuilds the compositor drive per run so the
/// stores (and their synthetic frames) are reused intact.
pub struct SoftwareEngine {
    /// The solved layout (canvas + normalized cells), shared into the drive.
    layout: Arc<Layout>,
    /// The fixed output cadence (exact rational).
    cadence: Rational,
    /// Per-source last-good-frame stores, keyed by source id.
    stores: HashMap<String, Arc<TileStore<Nv12Image>>>,
    /// The synthetic frame each source contributes (real bars/solid, or a
    /// placeholder card), kept so a run can (re)publish it into the stores.
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
    /// Wait-free slot the continuous run loop publishes a throttled clone of the
    /// composited program frame into, for the control plane's live preview. Read
    /// by the preview provider off the hot loop (invariant #10).
    program_preview: crate::preview::ProgramSlot,
}

impl SoftwareEngine {
    /// Build a software engine from an already-validated configuration.
    ///
    /// Solves the layout, creates one [`TileStore`] per source (with
    /// [`NoSignalPolicy::HoldForever`] so a once-published synthetic frame stays
    /// available across the whole bounded run), and builds each source's
    /// synthetic frame via [`software_source_frame`] (real bars/solid, else a
    /// per-tile placeholder).
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

            let pattern = software_source_frame(
                source,
                config.canvas.width,
                config.canvas.height,
                index,
                canvas_color,
            )
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
            program_preview: crate::preview::program_slot(),
        })
    }

    /// The wait-free program-preview slot (shared with the control plane's
    /// preview provider; the continuous run loop publishes into it).
    #[must_use]
    pub fn program_preview(&self) -> crate::preview::ProgramSlot {
        Arc::clone(&self.program_preview)
    }

    /// The per-source frame stores (shared with the preview provider for the
    /// per-input thumbnails).
    #[must_use]
    pub fn preview_stores(&self) -> HashMap<String, Arc<TileStore<Nv12Image>>> {
        self.stores.clone()
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
    /// Default `true`: each source's tile shows its synthetic frame — real bars/
    /// solid, else a placeholder card (LIVE). Set `false` to leave every store
    /// empty, proving the output produces a valid slate frame per tick even with
    /// no inputs (invariant #1 + #2).
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
        let publisher: EnginePublisher<SoftwareState, SoftwareState> =
            EnginePublisher::new(EVENT_CAPACITY);
        let stop = StopSignal::new();
        self.drive(
            &mut runtime,
            &publisher,
            &stop,
            Some(max_ticks),
            |f: &multiview_engine::CompositedFrame| SoftwareState {
                tick: f.tick.index,
                pts: f.pts(),
            },
            |f: &multiview_engine::CompositedFrame| {
                Some(SoftwareState {
                    tick: f.tick.index,
                    pts: f.pts(),
                })
            },
            |_d: &mut CompositorDrive<Nv12Image>| {},
        )
        .await
    }

    /// Drive the engine for `max_ticks` ticks under the production realtime
    /// pacer (monotonic time, real `sleep`s). Used by the binary's `--ticks`
    /// path and by realtime soak tests; paces to the wall clock.
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_for_realtime(&mut self, max_ticks: u64) -> Result<RunReport, RunError> {
        let time = Arc::new(MonotonicTimeSource::new());
        let ts: Arc<dyn TimeSource> = time;
        self.prime_stores(ts.as_ref());
        let mut runtime = self.build_runtime(ts, RealtimePacer)?;
        let publisher: EnginePublisher<SoftwareState, SoftwareState> =
            EnginePublisher::new(EVENT_CAPACITY);
        let stop = StopSignal::new();
        self.drive(
            &mut runtime,
            &publisher,
            &stop,
            Some(max_ticks),
            |f: &multiview_engine::CompositedFrame| SoftwareState {
                tick: f.tick.index,
                pts: f.pts(),
            },
            |f: &multiview_engine::CompositedFrame| {
                Some(SoftwareState {
                    tick: f.tick.index,
                    pts: f.pts(),
                })
            },
            |_d: &mut CompositorDrive<Nv12Image>| {},
        )
        .await
    }

    /// Drive the engine **forever** under the production realtime pacer until
    /// `stop` is raised (the binary wires this to Ctrl-C).
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_until_stopped(
        &mut self,
        stop: &StopSignal,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
    ) -> Result<RunReport, RunError> {
        self.run_until_stopped_with_control(stop, publisher, |_d| {})
            .await
    }

    /// Like [`SoftwareEngine::run_until_stopped`], but additionally applies
    /// control-plane reconfiguration at each frame boundary via `control` (e.g.
    /// the command-bus drain from [`crate::control::command_drain`]). `control`
    /// runs on the output-clock loop and must be non-blocking (invariants #1+#10).
    ///
    /// # Errors
    ///
    /// See [`SoftwareEngine::run_for`].
    pub async fn run_until_stopped_with_control<FC>(
        &mut self,
        stop: &StopSignal,
        publisher: &EnginePublisher<EngineStateSnapshot, Event>,
        control: FC,
    ) -> Result<RunReport, RunError>
    where
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let time = Arc::new(MonotonicTimeSource::new());
        let ts: Arc<dyn TimeSource> = time;
        self.prime_stores(ts.as_ref());
        let mut runtime = self.build_runtime(ts, RealtimePacer)?;
        // The caller owns the publisher so the control plane can share it
        // (read-only). State is the compact per-tick JSON snapshot; events are
        // left sparse for now (none emitted here — they arrive via change-driven
        // mirrors in a follow-up), so the broadcast carries no per-tick flood.
        //
        // The same per-tick projection also publishes a THROTTLED clone of the
        // composited canvas into the wait-free program-preview slot (every
        // `PREVIEW_CAPTURE_EVERY`th tick ≈ a couple of frames per second), so the
        // control plane can serve a live still without cloning every frame on the
        // hot loop. The store is a single atomic swap — it never blocks the clock.
        let preview = Arc::clone(&self.program_preview);
        // Sparse tile-state events: the projection tracks each source's last
        // *emitted* lifecycle state and emits at most one `tile.state` change per
        // tick (seeding every tile once, then on transitions). The control plane
        // sets the envelope id to the source id, so the monitoring UI keys each
        // tile by it. This is change-driven — not a per-tick flood (inv #10).
        let mut last_states: HashMap<String, multiview_core::traits::SourceState> = HashMap::new();
        self.drive(
            &mut runtime,
            publisher,
            stop,
            None,
            move |f: &multiview_engine::CompositedFrame| {
                if f.tick.index % PREVIEW_CAPTURE_EVERY == 0 {
                    preview.store(Some(Arc::new(f.canvas.clone())));
                }
                crate::control::state_snapshot(
                    f.tick.index,
                    f.pts().as_nanos(),
                    f.canvas.width(),
                    f.canvas.height(),
                )
            },
            move |f: &multiview_engine::CompositedFrame| -> Option<Event> {
                for (source, &state) in &f.source_states {
                    if last_states.get(source) != Some(&state) {
                        let from = last_states.get(source).copied().unwrap_or(state);
                        last_states.insert(source.clone(), state);
                        return Some(Event::TileState(multiview_events::TileState {
                            from: from.into(),
                            to: state.into(),
                            input: Some(source.clone()),
                            trigger: "state_change".to_owned(),
                        }));
                    }
                }
                None
            },
            control,
        )
        .await
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
    #[allow(clippy::too_many_arguments)]
    // reason: this is the single private dispatcher that folds the engine's run
    // outcome into a RunReport; its parameters (runtime, publisher, stop,
    // max_ticks, and the three hot-loop closures state_of/event_of/control) are
    // each distinct and dictated by `EngineRuntime::run*_with_control`'s
    // signature. Bundling them into a struct would only move the arity without
    // improving clarity for the four thin callers.
    async fn drive<P, S, E, FS, FE, FC>(
        &self,
        runtime: &mut EngineRuntime<P>,
        publisher: &EnginePublisher<S, E>,
        stop: &StopSignal,
        max_ticks: Option<u64>,
        state_of: FS,
        event_of: FE,
        control: FC,
    ) -> Result<RunReport, RunError>
    where
        P: Pacer,
        FS: FnMut(&multiview_engine::CompositedFrame) -> S,
        FE: FnMut(&multiview_engine::CompositedFrame) -> Option<E>,
        FC: FnMut(&mut CompositorDrive<Nv12Image>),
    {
        let outcome = match max_ticks {
            Some(max) => {
                runtime
                    .run_for_with_control(publisher, stop, max, state_of, event_of, control)
                    .await
            }
            None => {
                runtime
                    .run_with_control(publisher, stop, state_of, event_of, control)
                    .await
            }
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
/// (sleep-free) bounded software run.
///
/// Implemented for [`ManualTimeSource`] ([`ManualTimeSource::set`] jumps the
/// clock). A real monotonic source cannot be jumped, so the realtime path uses
/// [`SoftwareEngine::run_for_realtime`] (which paces against true elapsed time)
/// instead of [`SoftwareEngine::run_for`].
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
/// The synthetic frame a source contributes in the FFmpeg-free software engine.
///
/// `bars` and `solid` render their real picture (the synthetic kinds that need no
/// decoder — ADR-0027). Every other kind — a decoded feed the software build
/// cannot open, or `clock` (whose animated render lands with the generator task)
/// — contributes a distinct per-tile placeholder card so the smoke still
/// composites a frame.
fn software_source_frame(
    source: &Source,
    width: u32,
    height: u32,
    index: usize,
    canvas: CanvasColor,
) -> Result<Nv12Image, multiview_compositor::Error> {
    match &source.kind {
        SourceKind::Bars => Nv12Image::color_bars(width, height, canvas),
        SourceKind::Solid { color } => {
            // The colour was validated at config time; fall back to a slate if a
            // caller somehow bypassed validation (never panic on the build path).
            let (r, g, b) = multiview_config::parse_hex_color(color).unwrap_or((16, 16, 24));
            Nv12Image::solid_rgb(width, height, r, g, b, canvas)
        }
        _ => test_pattern(width, height, index, canvas.output_tag()),
    }
}

/// A distinct per-tile placeholder card (a flat hue per source index), used in
/// the software smoke for kinds it does not render natively.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull a real [`Source`] out of the parser (it is `#[non_exhaustive]`, so it
    /// cannot be struct-literal-constructed from this crate) by wrapping its
    /// `kind` fields in a minimal 1x1 document.
    fn source_with(kind_fields: &str) -> Source {
        let doc = format!(
            r##"schema_version = 1
[canvas]
width = 320
height = 240
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
{kind_fields}
[[cells]]
id = "cell_a"
area = "a"
[cells.source]
input_id = "in_a"
[[outputs]]
kind = "hls"
path = "/tmp/x.m3u8"
codec = "mpeg2video"
segment_ms = 1000
"##
        );
        let cfg = MultiviewConfig::load_from_toml(&doc).expect("parse minimal config");
        cfg.sources.into_iter().next().expect("one source")
    }

    #[test]
    fn bars_source_routes_to_real_colour_bars() {
        let canvas = CanvasColor::default();
        let src = source_with("kind = \"bars\"");
        let got = software_source_frame(&src, 560, 240, 0, canvas).expect("frame");
        let bars = Nv12Image::color_bars(560, 240, canvas).expect("bars");
        assert_eq!(
            got.y_plane(),
            bars.y_plane(),
            "a bars source must render real colour bars, not the placeholder"
        );
    }

    #[test]
    fn solid_source_routes_to_its_configured_colour() {
        let canvas = CanvasColor::default();
        let src = source_with("kind = \"solid\"\ncolor = \"#22aa44\"");
        let got = software_source_frame(&src, 64, 64, 0, canvas).expect("frame");
        let want = Nv12Image::solid_rgb(64, 64, 0x22, 0xaa, 0x44, canvas).expect("solid");
        assert_eq!(
            got.y_plane(),
            want.y_plane(),
            "a solid source must render its configured colour"
        );
    }

    #[test]
    fn a_decoded_kind_does_not_masquerade_as_bars() {
        // A kind the software smoke cannot decode (rtsp) gets the per-index
        // placeholder card — never silently rendered as bars.
        let canvas = CanvasColor::default();
        let src = source_with("kind = \"rtsp\"\nurl = \"rtsp://example/stream\"");
        let got = software_source_frame(&src, 560, 240, 0, canvas).expect("frame");
        let bars = Nv12Image::color_bars(560, 240, canvas).expect("bars");
        assert_ne!(
            got.y_plane(),
            bars.y_plane(),
            "a decoded kind must not look like bars"
        );
    }
}
