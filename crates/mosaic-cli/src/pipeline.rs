//! The **real** libav\* end-to-end `mosaic run` pipeline (the `ffmpeg` feature).
//!
//! Where [`crate::run`] is the pure-software, FFmpeg-free smoke of invariant #1,
//! this module makes Mosaic *operable*: it ingests real video, composites it on
//! the CPU reference compositor driven by the engine's protected output core,
//! encodes the canvas **once**, and fans the encoded program out to the file and
//! HLS output sinks declared in the config.
//!
//! ```text
//! config ─▶ decode each source to NV12 (scaled to its cell)  ─┐
//!                                                              ├▶ per-tile TileStore
//!  OutputClock ─▶ EngineRuntime drive (CPU compositor) ───────┘   (sampled, never paced)
//!       │
//!       └▶ one composited canvas per tick (out_pts = f(tick))
//!                                                              ├▶ encode ONCE (mpeg2video
//!                                                              │   by default; libx264 under
//!                                                              │   gpl-codecs)
//!                                                              └▶ fan out: FileSink + SegmentSink
//! ```
//!
//! ## Invariants upheld
//!
//! * **#1 output-clock.** The engine's [`EngineRuntime`](mosaic_engine::EngineRuntime)
//!   emits exactly one composited frame per tick; a source that ran out of
//!   frames simply holds its last-good frame (or shows the slate) — it never
//!   stalls the loop.
//! * **#3 timing.** PTS is re-stamped from the tick counter by the output sinks
//!   (`out_pts = tick`); raw input PTS is never forwarded to an encoder/muxer.
//! * **#5 NV12-throughout.** Frames stay NV12 from decode through composite; the
//!   one NV12 → encoder-format (`yuv420p`) conversion happens inside the sink,
//!   immediately before `send_frame`.
//! * **#7 encode-once-mux-many.** The canvas is composited once per tick and
//!   encoded once per rendition; the file and HLS sinks each consume the *same*
//!   composited frames.
//! * **Licensing.** The default encode codec is LGPL `mpeg2video`; the GPL
//!   `libx264` path is reachable only when the crate is built with
//!   `gpl-codecs`. No FFI lives here — the crate stays `unsafe_code = forbid`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ffmpeg_next as ffmpeg;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;

use mosaic_compositor::blend::LinearRgba;
use mosaic_compositor::pipeline::{CanvasColor, Nv12Image};
use mosaic_config::{MosaicConfig, Output, Source, SourceKind};
use mosaic_core::frame::FrameMeta;
use mosaic_core::layout::{Cell, Layout};
use mosaic_core::pixel::PixelFormat;
use mosaic_core::time::{MediaTime, Rational};
use mosaic_engine::{
    CompositedFrame, CompositorDrive, EnginePublisher, EngineRuntime, MonotonicTimeSource,
    OutputClock, Pacer, RealtimePacer, StopSignal, TimeSource,
};
use mosaic_ffmpeg::{DecodedVideoFrame, ScaleSpec, Scaler, StreamVideoDecoder};
use mosaic_framestore::{NoSignalPolicy, TileStore, TileThresholds};
use mosaic_output::sink::{EncodeConfig, FileSink, SegmentSink, VideoFrameSource};

/// The per-subscriber drop-oldest depth of the engine's outbound event stream.
/// The pipeline has no realtime consumers wired here, but the publisher still
/// needs a positive ring (invariant #10).
const EVENT_CAPACITY: usize = 64;

/// Errors building or running the real libav\* pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PipelineError {
    /// The configuration failed to solve into a layout.
    #[error("invalid configuration: {0}")]
    Config(#[from] mosaic_config::ConfigError),
    /// An ingest source could not be opened/decoded.
    #[error("ingest source {id:?}: {reason}")]
    Ingest {
        /// The source id that failed.
        id: String,
        /// The underlying reason.
        reason: String,
    },
    /// The output clock rejected the canvas cadence.
    #[error("output clock: {0}")]
    Clock(String),
    /// The engine drive/compositor rejected the canvas.
    #[error("engine: {0}")]
    Engine(String),
    /// A composited canvas could not be bridged to a libav frame.
    #[error("frame bridge: {0}")]
    Bridge(String),
    /// An output sink failed.
    #[error("output {kind}: {reason}")]
    Output {
        /// The output kind that failed.
        kind: &'static str,
        /// The underlying reason.
        reason: String,
    },
    /// No runnable output sink was declared (or supported) in the config.
    #[error("config declares no runnable file/HLS output (only {0} are wired today)")]
    NoOutput(&'static str),
    /// The requested codec is not encodable in this build (e.g. H.264 without
    /// the `gpl-codecs` feature).
    #[error("codec {codec:?} cannot be encoded by this build: {reason}")]
    Codec {
        /// The requested logical codec.
        codec: String,
        /// Why it is unavailable.
        reason: String,
    },
}

/// A summary of one real pipeline run.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PipelineReport {
    /// Composited canvas frames driven by the output clock (== ticks).
    pub frames: u64,
    /// The fixed output cadence (exact rational).
    pub cadence: Rational,
    /// Canvas width in pixels.
    pub canvas_width: u32,
    /// Canvas height in pixels.
    pub canvas_height: u32,
    /// The concrete libav encoder used (e.g. `"mpeg2video"`, `"libx264"`).
    pub encoder: String,
    /// Per-output artifacts written, one line each (path + encoded packet count).
    pub outputs: Vec<String>,
    /// Whether the output ever faltered (`frames != ticks`). Must be `false`.
    pub faltered: bool,
}

impl PipelineReport {
    /// Render the report as the multi-line text the binary prints.
    #[must_use]
    pub fn render(&self) -> String {
        let verdict = if self.faltered {
            "FALTERED"
        } else {
            "never faltered"
        };
        let mut lines = vec![format!(
            "run (ffmpeg): {} frame(s) at {}/{} fps on {}x{}; encoder {}; output {}",
            self.frames,
            self.cadence.num,
            self.cadence.den,
            self.canvas_width,
            self.canvas_height,
            self.encoder,
            verdict,
        )];
        for out in &self.outputs {
            lines.push(format!("  - {out}"));
        }
        lines.join("\n")
    }
}

/// The logical video codec the configured outputs request, resolved to a
/// concrete libav encoder this build is allowed to use.
struct ResolvedEncoder {
    /// The concrete libav encoder short name (`"mpeg2video"`, `"libx264"`, …).
    name: String,
    /// The pixel format the encoder is fed (`yuv420p` for the software codecs).
    format: Pixel,
}

/// Map a config output `codec` token to a logical [`mosaic_ffmpeg::VideoCodec`].
/// Unknown / unsupported tokens fall back to MPEG-2 (the LGPL-clean default).
fn logical_codec(token: &str) -> mosaic_ffmpeg::VideoCodec {
    use mosaic_ffmpeg::VideoCodec;
    match token.to_ascii_lowercase().as_str() {
        "h264" | "avc" => VideoCodec::H264,
        "h265" | "hevc" => VideoCodec::H265,
        "ffv1" => VideoCodec::Ffv1,
        "mjpeg" => VideoCodec::Mjpeg,
        _ => VideoCodec::Mpeg2Video,
    }
}

/// Resolve the encoder to use for `codec_token`, honouring the compiled features
/// (LGPL `mpeg2video` by default; `libx264`/`libx265` under `gpl-codecs`).
///
/// If the requested codec cannot be encoded by this build (e.g. H.264 with no
/// `gpl-codecs`), this falls back to MPEG-2 so a config that names `h264` still
/// produces a real, playable file in an LGPL-clean build — and logs the
/// substitution. Returns [`PipelineError::Codec`] only if even the fallback is
/// somehow unavailable in the linked `FFmpeg`.
fn resolve_encoder(codec_token: &str) -> Result<ResolvedEncoder, PipelineError> {
    use mosaic_ffmpeg::{select_encoder, VideoCodec};
    let requested = logical_codec(codec_token);
    if let Some(name) = select_encoder(requested) {
        return Ok(ResolvedEncoder {
            name: name.to_owned(),
            format: Pixel::YUV420P,
        });
    }
    // The requested codec is not encodable in this build; fall back to MPEG-2.
    tracing::warn!(
        requested = ?requested,
        "requested output codec is not encodable in this build (need gpl-codecs/cuda); \
         falling back to LGPL mpeg2video"
    );
    let fallback = select_encoder(VideoCodec::Mpeg2Video).ok_or_else(|| PipelineError::Codec {
        codec: codec_token.to_owned(),
        reason: "neither the requested codec nor the mpeg2video fallback is present in FFmpeg"
            .to_owned(),
    })?;
    Ok(ResolvedEncoder {
        name: fallback.to_owned(),
        format: Pixel::YUV420P,
    })
}

/// A runnable output sink resolved from a config `[[outputs]]` entry.
enum RunnableOutput {
    /// A single container file (codec/extension from the config path).
    File(FileSink),
    /// An HLS segmenter writing `seg*.ts` + a media playlist into a directory.
    Hls {
        /// The segmenter.
        sink: SegmentSink,
        /// Where the `.m3u8` playlist is written.
        playlist_path: PathBuf,
    },
}

/// A built, ready-to-run real pipeline.
pub struct RealPipeline {
    /// The solved layout (canvas + normalized cells).
    layout: Arc<Layout>,
    /// The fixed output cadence (exact rational).
    cadence: Rational,
    /// Per-source last-good-frame stores, keyed by source id.
    stores: HashMapStores,
    /// Decoded NV12 frame sequences per source (scaled to the bound cell size),
    /// keyed by source id; the drive loop publishes the next frame per tick.
    decoded: HashMapFrames,
    /// The fixed canvas color (ADR-C001 SDR BT.709 limited).
    canvas_color: CanvasColor,
    /// The "no signal" slate composited for tiles with no usable frame.
    nosignal_card: Nv12Image,
    /// The canvas background shown where no tile covers.
    background: LinearRgba,
    /// The resolved concrete encoder (name + fed pixel format).
    encoder: ResolvedEncoder,
    /// The runnable outputs declared in the config.
    outputs: Vec<RunnableOutput>,
}

type HashMapStores = std::collections::HashMap<String, Arc<TileStore<Nv12Image>>>;
type HashMapFrames = std::collections::HashMap<String, Vec<Arc<Nv12Image>>>;

impl RealPipeline {
    /// Build the real pipeline from an already-validated configuration.
    ///
    /// Solves the layout, opens + decodes every declared source into NV12 frames
    /// scaled to the cell it binds, resolves the output encoder (LGPL by
    /// default), and builds the runnable file/HLS sinks.
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the layout cannot be solved, a source
    /// cannot be opened/decoded, no runnable output is declared, or the encoder
    /// cannot be resolved.
    pub fn build(config: &MosaicConfig) -> Result<Self, PipelineError> {
        let layout = Arc::new(config.solve_layout()?);
        let cadence = config.canvas.fps.rational();
        let canvas_color = CanvasColor::default();
        let tag = canvas_color.output_tag();

        // Map each source id to the pixel size of the cell that binds it, so the
        // decoded frames are scaled to tile the canvas (the reference compositor
        // places tiles 1:1 at the cell origin).
        let mut stores: HashMapStores = std::collections::HashMap::new();
        let mut decoded: HashMapFrames = std::collections::HashMap::new();

        for source in &config.sources {
            let (tile_w, tile_h) = cell_pixel_size(&layout, &source.id)
                .unwrap_or((config.canvas.width, config.canvas.height));
            let frames = decode_source(source, tile_w, tile_h, tag)?;
            let store = Arc::new(TileStore::new(
                source.id.clone(),
                TileThresholds::default(),
                NoSignalPolicy::HoldForever,
            ));
            stores.insert(source.id.clone(), store);
            decoded.insert(source.id.clone(), frames);
        }

        let nosignal_card =
            Nv12Image::solid(config.canvas.width, config.canvas.height, 16, 128, 128, tag)
                .map_err(|e| PipelineError::Engine(e.to_string()))?;

        // Resolve the encoder from the first output that names a codec (file/HLS
        // share one encode — invariant #7). Default to MPEG-2 if none names one.
        let codec_token = config
            .outputs
            .iter()
            .find_map(output_codec)
            .unwrap_or("mpeg2video");
        let encoder = resolve_encoder(codec_token)?;

        // One-second GOP (rounded), also the per-segment frame count for HLS.
        let gop = ticks_per_second(cadence).max(1);

        let cfg = EncodeConfig {
            codec_name: encoder.name.clone(),
            width: config.canvas.width,
            height: config.canvas.height,
            format: encoder.format,
            cadence,
            gop,
            bit_rate: 4_000_000,
        };

        let outputs = build_outputs(&config.outputs, &cfg)?;
        if outputs.is_empty() {
            return Err(PipelineError::NoOutput("file/HLS"));
        }

        Ok(Self {
            layout,
            cadence,
            stores,
            decoded,
            canvas_color,
            nosignal_card,
            background: LinearRgba::opaque(0.02, 0.02, 0.05),
            encoder,
            outputs,
        })
    }

    /// The fixed output cadence (exact rational).
    #[must_use]
    pub const fn cadence(&self) -> Rational {
        self.cadence
    }

    /// The number of ingest sources wired into this pipeline.
    #[must_use]
    pub fn source_count(&self) -> usize {
        self.stores.len()
    }

    /// The resolved concrete encoder name.
    #[must_use]
    pub fn encoder_name(&self) -> &str {
        &self.encoder.name
    }

    /// Run the engine for exactly `max_ticks` ticks under the realtime pacer,
    /// then encode the composited program once and fan it out to every
    /// configured sink.
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the clock/engine reject the canvas, or any
    /// sink fails. Input exhaustion is **not** an error: a source past its last
    /// frame holds its last-good frame, so the output keeps emitting on cadence.
    pub async fn run_for(&mut self, max_ticks: u64) -> Result<PipelineReport, PipelineError> {
        let time = Arc::new(MonotonicTimeSource::new());
        let stop = StopSignal::new();
        self.drive(time, RealtimePacer, &stop, Some(max_ticks))
            .await
    }

    /// Run the engine **until `stop`** under the realtime pacer (the binary wires
    /// `stop` to Ctrl-C), then encode + fan out.
    ///
    /// # Errors
    ///
    /// See [`RealPipeline::run_for`].
    pub async fn run_until(&mut self, stop: &StopSignal) -> Result<PipelineReport, PipelineError> {
        let time = Arc::new(MonotonicTimeSource::new());
        self.drive(time, RealtimePacer, stop, None).await
    }

    /// Drive the engine's protected output core for the bounded run, collecting
    /// one composited canvas per tick, then encode + mux all outputs.
    async fn drive<P: Pacer>(
        &mut self,
        time: Arc<MonotonicTimeSource>,
        pacer: P,
        stop: &StopSignal,
        max_ticks: Option<u64>,
    ) -> Result<PipelineReport, PipelineError> {
        let clock =
            OutputClock::new(self.cadence).map_err(|e| PipelineError::Clock(e.to_string()))?;
        let drive = CompositorDrive::new(
            Arc::clone(&self.layout),
            self.stores.clone(),
            self.nosignal_card.clone(),
            self.canvas_color,
            self.background,
        )
        .map_err(|e| PipelineError::Engine(e.to_string()))?;

        let ts: Arc<dyn TimeSource> = time;
        let mut runtime = EngineRuntime::new(clock, drive, ts, pacer);
        let publisher: EnginePublisher<TickState, TickState> = EnginePublisher::new(EVENT_CAPACITY);

        // Collected composited canvases (Arc to avoid copies), in tick order.
        let collected: Arc<Mutex<Vec<Arc<Nv12Image>>>> = Arc::new(Mutex::new(Vec::new()));

        // The projection runs once per tick on the hot loop. It (a) advances each
        // source store to the NEXT decoded frame (so the next tick samples a fresh
        // frame — inputs are sampled, never pacing), and (b) snapshots the canvas
        // into the collection. Both are cheap, panic-free, and never block the
        // engine (the collection lock is held only for a push; invariant #10 is
        // not at risk because this is a bounded offline collection, not a realtime
        // consumer that could back-pressure a live engine).
        let stores = self.stores.clone();
        let decoded = self.decoded.clone();
        let collect = Arc::clone(&collected);
        let state_of = move |frame: &CompositedFrame| -> TickState {
            // Publish the frame this tick "consumed" plus prime the next one:
            // advance each store to frame index `tick.index` of its sequence,
            // holding the last frame once the sequence is exhausted (invariant
            // #1 — output never stalls on input exhaustion).
            let index = frame.tick.index;
            for (id, frames) in &decoded {
                if let Some(store) = stores.get(id) {
                    if let Some(img) = pick_frame(frames, index) {
                        store.publish_arc(Arc::clone(img), frame.pts());
                    }
                }
            }
            if let Ok(mut sink) = collect.lock() {
                sink.push(Arc::new(frame.canvas.clone()));
            }
            TickState {
                tick: index,
                pts: frame.pts(),
            }
        };
        let event_of = |frame: &CompositedFrame| TickState {
            tick: frame.tick.index,
            pts: frame.pts(),
        };

        // Prime tick 0's frames into the stores BEFORE the loop so the first
        // composite samples a real first frame rather than the slate.
        self.prime_tick_zero();

        let outcome = match max_ticks {
            Some(max) => {
                runtime
                    .run_for(&publisher, stop, max, state_of, event_of)
                    .await
            }
            None => runtime.run(&publisher, stop, state_of, event_of).await,
        }
        .map_err(|e| PipelineError::Engine(e.to_string()))?;

        let frames = match collected.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let collected_count = u64::try_from(frames.len()).unwrap_or(u64::MAX);
        let faltered = collected_count != outcome.ticks;

        let output_lines = self.encode_outputs(&frames)?;

        Ok(PipelineReport {
            frames: outcome.ticks,
            cadence: self.cadence,
            canvas_width: self.layout.canvas.width,
            canvas_height: self.layout.canvas.height,
            encoder: self.encoder.name.clone(),
            outputs: output_lines,
            faltered,
        })
    }

    /// Publish each source's first decoded frame into its store at t=0 so the
    /// engine's first composite (tick 0) samples a real frame.
    fn prime_tick_zero(&self) {
        for (id, frames) in &self.decoded {
            if let Some(store) = self.stores.get(id) {
                if let Some(img) = pick_frame(frames, 0) {
                    store.publish_arc(Arc::clone(img), MediaTime::ZERO);
                }
            }
        }
    }

    /// Encode the collected composited canvases once and fan them out to every
    /// configured sink (file + HLS), returning one human line per output.
    fn encode_outputs(&self, frames: &[Arc<Nv12Image>]) -> Result<Vec<String>, PipelineError> {
        let mut lines = Vec::with_capacity(self.outputs.len());
        for output in &self.outputs {
            match output {
                RunnableOutput::File(sink) => {
                    let mut source = CanvasFrameSource::new(frames);
                    let stats = sink.run(&mut source).map_err(|e| PipelineError::Output {
                        kind: "file",
                        reason: e.to_string(),
                    })?;
                    lines.push(format!(
                        "file {}: {} packet(s), {} keyframe(s)",
                        sink.path().display(),
                        stats.packets,
                        stats.keyframes
                    ));
                }
                RunnableOutput::Hls {
                    sink,
                    playlist_path,
                } => {
                    let mut source = CanvasFrameSource::new(frames);
                    let result = sink.run(&mut source).map_err(|e| PipelineError::Output {
                        kind: "hls",
                        reason: e.to_string(),
                    })?;
                    let playlist_text = result.playlist.render();
                    std::fs::write(playlist_path, playlist_text.as_bytes()).map_err(|e| {
                        PipelineError::Output {
                            kind: "hls",
                            reason: format!("writing playlist {}: {e}", playlist_path.display()),
                        }
                    })?;
                    lines.push(format!(
                        "hls {} + {} segment(s) ({} packet(s))",
                        playlist_path.display(),
                        result.segments.len(),
                        result.stats.packets
                    ));
                }
            }
        }
        Ok(lines)
    }
}

/// The per-tick state snapshot published outward (invariant #10): the tick index
/// and its presentation timestamp. Best-effort; no consumer can back-pressure it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TickState {
    tick: u64,
    pts: MediaTime,
}

/// Pick the decoded frame for tick `index`, holding the last frame once the
/// sequence is exhausted (so a finite clip loops to a freeze rather than the
/// slate — input exhaustion never stalls the output, invariant #1).
fn pick_frame(frames: &[Arc<Nv12Image>], index: u64) -> Option<&Arc<Nv12Image>> {
    if frames.is_empty() {
        return None;
    }
    let i = usize::try_from(index).unwrap_or(usize::MAX);
    frames.get(i).or_else(|| frames.last())
}

/// The pixel size of the cell that binds `source_id`, if any.
fn cell_pixel_size(layout: &Layout, source_id: &str) -> Option<(u32, u32)> {
    let cell = layout
        .cells
        .iter()
        .find(|c| c.source.as_deref() == Some(source_id))?;
    Some(cell_dims(cell, layout.canvas.width, layout.canvas.height))
}

/// Convert a cell's normalized `w`/`h` into even pixel dimensions (NV12 needs
/// even extents), clamped to at least 2x2.
fn cell_dims(cell: &Cell, canvas_w: u32, canvas_h: u32) -> (u32, u32) {
    let to_even_px = |frac: f32, extent: u32| -> u32 {
        let raw = frac_to_px(frac, extent);
        let even = raw & !1; // round down to even
        even.max(2)
    };
    (to_even_px(cell.w, canvas_w), to_even_px(cell.h, canvas_h))
}

/// Convert a normalized fraction in `[0,1]` to a pixel count in `[0, extent]`,
/// `as`-cast-free (the guardrails deny `as_conversions`).
fn frac_to_px(frac: f32, extent: u32) -> u32 {
    if !frac.is_finite() || frac <= 0.0 {
        return 0;
    }
    let target = f64::from(frac) * f64::from(extent);
    if target >= f64::from(extent) {
        return extent;
    }
    // Largest u32 candidate whose lossless f64 widening is <= target.
    let mut lo: u32 = 0;
    let mut hi: u32 = extent;
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f64::from(mid) <= target {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    lo
}

/// Whole ticks in one second at `cadence` (`num/den`), rounded to nearest, used
/// as the GOP / segment length. Exact integer arithmetic (never float fps).
fn ticks_per_second(cadence: Rational) -> u32 {
    let num = i128::from(cadence.num);
    let den = i128::from(cadence.den).max(1);
    let rounded = (num + den / 2) / den;
    u32::try_from(rounded.max(1)).unwrap_or(u32::MAX)
}

/// The codec token a config output names, if it carries one.
fn output_codec(output: &Output) -> Option<&str> {
    match output {
        Output::RtspServer { codec, .. }
        | Output::LlHls { codec, .. }
        | Output::Hls { codec, .. }
        | Output::Rtmp { codec, .. }
        | Output::Srt { codec, .. } => Some(codec.as_str()),
        // NDI carries no codec token; a future output kind names none here until
        // explicitly wired (`Output` is `#[non_exhaustive]`).
        Output::Ndi { .. } | _ => None,
    }
}

/// Build the runnable file/HLS sinks from the config outputs. RTSP/NDI/RTMP/SRT
/// transports are not run from the CLI yet (the servers are feature-gated
/// scaffolds); they are skipped with a log line rather than failing the run, so
/// a config mixing a server with a file/HLS output still produces a real file.
fn build_outputs(
    outputs: &[Output],
    cfg: &EncodeConfig,
) -> Result<Vec<RunnableOutput>, PipelineError> {
    let mut runnable = Vec::new();
    for output in outputs {
        match output {
            Output::Hls { path, .. } | Output::LlHls { path, .. } => {
                let (dir, prefix, playlist_path) = hls_paths(Path::new(path));
                std::fs::create_dir_all(&dir).map_err(|e| PipelineError::Output {
                    kind: "hls",
                    reason: format!("creating {}: {e}", dir.display()),
                })?;
                runnable.push(RunnableOutput::Hls {
                    sink: SegmentSink::new(cfg.clone(), dir, prefix),
                    playlist_path,
                });
            }
            Output::RtspServer { .. } => {
                tracing::warn!("rtsp_server output is not yet runnable from the CLI; skipping");
            }
            Output::Ndi { .. } => {
                tracing::warn!("ndi output is not yet runnable from the CLI; skipping");
            }
            Output::Rtmp { .. } | Output::Srt { .. } => {
                tracing::warn!("rtmp/srt push outputs are not yet runnable from the CLI; skipping");
            }
            // `Output` is `#[non_exhaustive]`; an unrecognized future kind is
            // skipped rather than silently mishandled.
            _ => {
                tracing::warn!("unrecognized output kind is not runnable from the CLI; skipping");
            }
        }
    }
    // The config schema has no `file` output kind; a single-file artifact is
    // derived from the FIRST HLS output's directory (program.<ext>) so the run
    // always writes a self-contained playable container alongside the segments.
    // If no HLS output exists, there is nothing runnable.
    Ok(prepend_file_sink(runnable, cfg))
}

/// Derive a single-file container sink from the first HLS output (same encode),
/// so a `run` always produces a self-contained playable file in addition to the
/// segmented playlist (encode-once-mux-many — invariant #7).
fn prepend_file_sink(mut runnable: Vec<RunnableOutput>, cfg: &EncodeConfig) -> Vec<RunnableOutput> {
    let file_path = runnable.iter().find_map(|r| match r {
        RunnableOutput::Hls { playlist_path, .. } => {
            Some(playlist_path.with_file_name("program.ts"))
        }
        RunnableOutput::File(_) => None,
    });
    if let Some(path) = file_path {
        runnable.insert(0, RunnableOutput::File(FileSink::new(cfg.clone(), path)));
    }
    runnable
}

/// Split an HLS output `path` into `(segment_dir, segment_prefix, playlist_path)`.
///
/// A `path` ending in `.m3u8` names the playlist; segments are written beside it
/// with prefix `seg`. A `path` naming a directory writes `index.m3u8` + `seg*.ts`
/// inside it.
fn hls_paths(path: &Path) -> (PathBuf, String, PathBuf) {
    if path.extension().and_then(std::ffi::OsStr::to_str) == Some("m3u8") {
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        (dir, "seg".to_owned(), path.to_path_buf())
    } else {
        (
            path.to_path_buf(),
            "seg".to_owned(),
            path.join("index.m3u8"),
        )
    }
}

/// A [`VideoFrameSource`] over the collected composited canvases: it bridges each
/// `Nv12Image` into a libav NV12 [`Video`] frame for the output sinks. The
/// frame's PTS is left for the sink to re-stamp from the tick counter (the sinks
/// overwrite it; invariant #3).
struct CanvasFrameSource<'a> {
    frames: &'a [Arc<Nv12Image>],
    next: usize,
}

impl<'a> CanvasFrameSource<'a> {
    fn new(frames: &'a [Arc<Nv12Image>]) -> Self {
        Self { frames, next: 0 }
    }
}

impl VideoFrameSource for CanvasFrameSource<'_> {
    fn next_frame(&mut self) -> mosaic_output::Result<Option<DecodedVideoFrame>> {
        let Some(image) = self.frames.get(self.next) else {
            return Ok(None);
        };
        self.next = self.next.saturating_add(1);
        let frame = nv12_to_video(image)
            .map_err(|e| mosaic_output::Error::Output(format!("canvas bridge: {e}")))?;
        let meta = FrameMeta {
            pts: MediaTime::ZERO,
            width: image.width(),
            height: image.height(),
            format: PixelFormat::Nv12,
            color: image.color(),
        };
        Ok(Some(DecodedVideoFrame { frame, meta }))
    }
}

/// Bridge a CPU-reference [`Nv12Image`] into a libav NV12 [`Video`] frame by
/// copying its planes row-by-row into the freshly-allocated frame's planes
/// (respecting libav's plane strides, which may be padded). No FFI: this uses
/// only `ffmpeg-next`'s safe `Video::new`/`data_mut`/`stride` value API.
fn nv12_to_video(image: &Nv12Image) -> Result<Video, PipelineError> {
    let w = image.width();
    let h = image.height();
    let mut frame = Video::new(Pixel::NV12, w, h);

    let wu = usize::try_from(w).map_err(|_| PipelineError::Bridge("width overflow".to_owned()))?;
    let hu = usize::try_from(h).map_err(|_| PipelineError::Bridge("height overflow".to_owned()))?;

    // Read strides before taking the mutable plane borrows (the immutable
    // `stride` borrow cannot overlap the `data_mut` borrow).
    let y_stride = frame_stride(&frame, 0)?;
    let uv_stride = frame_stride(&frame, 1)?;

    // Plane 0: Y, `w` bytes per row, `h` rows.
    copy_plane(frame.data_mut(0), y_stride, image.y_plane(), wu, hu)?;
    // Plane 1: interleaved Cb/Cr, `w` bytes per row, `h/2` rows.
    copy_plane(frame.data_mut(1), uv_stride, image.uv_plane(), wu, hu / 2)?;
    Ok(frame)
}

/// The libav stride (bytes per row) of `frame`'s `plane`.
fn frame_stride(frame: &Video, plane: usize) -> Result<usize, PipelineError> {
    let stride = frame.stride(plane);
    if stride == 0 {
        return Err(PipelineError::Bridge(format!(
            "plane {plane} has zero stride"
        )));
    }
    Ok(stride)
}

/// Copy `rows` rows of `row_bytes` bytes from a tightly-packed `src` into a
/// libav plane `dst` whose rows are `dst_stride` bytes apart (`dst_stride` >=
/// `row_bytes`; the trailing padding is left untouched).
fn copy_plane(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    row_bytes: usize,
    rows: usize,
) -> Result<(), PipelineError> {
    if dst_stride < row_bytes {
        return Err(PipelineError::Bridge(
            "libav plane stride is narrower than the row".to_owned(),
        ));
    }
    for row in 0..rows {
        let src_start = row
            .checked_mul(row_bytes)
            .ok_or_else(|| PipelineError::Bridge("src offset overflow".to_owned()))?;
        let dst_start = row
            .checked_mul(dst_stride)
            .ok_or_else(|| PipelineError::Bridge("dst offset overflow".to_owned()))?;
        let src_row = src
            .get(src_start..src_start + row_bytes)
            .ok_or_else(|| PipelineError::Bridge("src row out of range".to_owned()))?;
        let dst_row = dst
            .get_mut(dst_start..dst_start + row_bytes)
            .ok_or_else(|| PipelineError::Bridge("dst row out of range".to_owned()))?;
        dst_row.copy_from_slice(src_row);
    }
    Ok(())
}

/// Decode a config [`Source`] into a sequence of NV12 frames scaled to
/// `(tile_w, tile_h)`, tagged like the canvas.
///
/// `test` sources are generated with the `ffmpeg` CLI (LGPL `testsrc` →
/// `mpeg2video`); file/rtsp/hls/ts/srt/rtmp sources are opened by URL/path. All
/// are decoded to NV12 via `mosaic-ffmpeg`'s safe `StreamVideoDecoder` and
/// resampled to the tile size.
fn decode_source(
    source: &Source,
    tile_w: u32,
    tile_h: u32,
    tag: mosaic_core::color::ColorInfo,
) -> Result<Vec<Arc<Nv12Image>>, PipelineError> {
    let owned_path; // keeps a generated test clip's tempdir alive for the decode
    let location: SourceLocation = match &source.kind {
        SourceKind::Test => {
            owned_path =
                generate_test_clip(&source.id).map_err(|reason| PipelineError::Ingest {
                    id: source.id.clone(),
                    reason,
                })?;
            SourceLocation::Path(owned_path.0.clone())
        }
        SourceKind::File { path } => SourceLocation::Path(PathBuf::from(path)),
        SourceKind::Rtsp { url, .. }
        | SourceKind::Hls { url }
        | SourceKind::Ts { url }
        | SourceKind::Srt { url }
        | SourceKind::Rtmp { url } => SourceLocation::Url(url.clone()),
        SourceKind::Ndi { .. } => {
            return Err(PipelineError::Ingest {
                id: source.id.clone(),
                reason: "NDI ingest is not wired in the CLI yet".to_owned(),
            })
        }
        // `SourceKind` is `#[non_exhaustive]`; a future kind is unsupported here
        // until explicitly wired (never silently mishandled).
        _ => {
            return Err(PipelineError::Ingest {
                id: source.id.clone(),
                reason: "unsupported source kind for the CLI pipeline".to_owned(),
            })
        }
    };

    let frames =
        decode_to_nv12(&location, tile_w, tile_h, tag).map_err(|reason| PipelineError::Ingest {
            id: source.id.clone(),
            reason,
        })?;
    if frames.is_empty() {
        return Err(PipelineError::Ingest {
            id: source.id.clone(),
            reason: "source decoded to zero frames".to_owned(),
        });
    }
    Ok(frames.into_iter().map(Arc::new).collect())
}

/// Where a source's media lives.
enum SourceLocation {
    /// A local filesystem path.
    Path(PathBuf),
    /// A libav-openable URL (rtsp/hls/ts/srt/rtmp).
    Url(String),
}

/// A generated test clip plus the tempdir that owns it (kept alive until decode
/// completes).
struct GeneratedClip(PathBuf, #[allow(dead_code)] tempfile::TempDir);

/// Generate a small LGPL `testsrc` clip for a `test` source. Uses `mpeg2video`
/// (LGPL, in-tree) — never x264/x265 — so generation stays LGPL-clean.
fn generate_test_clip(id: &str) -> Result<GeneratedClip, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let clip = dir.path().join(format!("test-{id}.ts"));
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x480:rate=25:duration=10",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            "mpeg2video",
            "-g",
            "25",
            "-f",
            "mpegts",
        ])
        .arg(&clip)
        .status()
        .map_err(|e| format!("spawning ffmpeg CLI: {e}"))?;
    if !status.success() {
        return Err("ffmpeg CLI failed to generate the test clip".to_owned());
    }
    if !clip.exists() {
        return Err("ffmpeg CLI produced no test clip".to_owned());
    }
    Ok(GeneratedClip(clip, dir))
}

/// Open `location`, decode its best video stream to NV12 frames scaled to
/// `(tile_w, tile_h)`, tag them like the canvas, and collect them.
///
/// Uses `ffmpeg-next`'s safe `Input`/`Parameters` value types only to bridge the
/// container's stream parameters into `mosaic-ffmpeg`'s safe `StreamVideoDecoder`
/// (which `mosaic-ffmpeg`'s `Demuxer` does not yet surface). No `unsafe`, no FFI.
fn decode_to_nv12(
    location: &SourceLocation,
    tile_w: u32,
    tile_h: u32,
    tag: mosaic_core::color::ColorInfo,
) -> Result<Vec<Nv12Image>, String> {
    mosaic_ffmpeg::ensure_initialized().map_err(|e| e.to_string())?;

    let mut input = match location {
        SourceLocation::Path(p) => ffmpeg::format::input(p).map_err(|e| e.to_string())?,
        SourceLocation::Url(u) => ffmpeg::format::input(&u.as_str()).map_err(|e| e.to_string())?,
    };

    let (stream_index, params, time_base) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| "input has no video stream".to_owned())?;
        (
            stream.index(),
            stream.parameters(),
            mosaic_ffmpeg::from_ff_rational(stream.time_base()),
        )
    };

    let mut decoder = StreamVideoDecoder::new(params, time_base).map_err(|e| e.to_string())?;
    let mut to_tile = TileScaler::new(tile_w, tile_h);
    let mut frames = Vec::new();

    // Pump packets, draining decoded frames, then flush at EOF.
    let mut drained = false;
    loop {
        while let Some(decoded) = decoder.receive_frame().map_err(|e| e.to_string())? {
            frames.push(to_tile.convert(&decoded.frame, tag)?);
        }
        if drained {
            break;
        }
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {
                if packet.stream() == stream_index {
                    decoder.send_packet(&packet).map_err(|e| e.to_string())?;
                }
            }
            Err(ffmpeg::Error::Eof) => {
                decoder.send_eof().map_err(|e| e.to_string())?;
                drained = true;
            }
            Err(other) => return Err(other.to_string()),
        }
    }
    Ok(frames)
}

/// Lazily-built scaler from a decoded NV12 frame's geometry to the tile size,
/// emitting an [`Nv12Image`] tagged like the canvas.
struct TileScaler {
    tile_w: u32,
    tile_h: u32,
    scaler: Option<Scaler>,
}

impl TileScaler {
    fn new(tile_w: u32, tile_h: u32) -> Self {
        Self {
            tile_w,
            tile_h,
            scaler: None,
        }
    }

    /// Scale `frame` (NV12 or P010 host frame) to the tile size as NV12 and wrap
    /// it as a tagged [`Nv12Image`].
    fn convert(
        &mut self,
        frame: &Video,
        tag: mosaic_core::color::ColorInfo,
    ) -> Result<Nv12Image, String> {
        let src = ScaleSpec::new(frame.format(), frame.width(), frame.height());
        let dst = ScaleSpec::new(Pixel::NV12, self.tile_w, self.tile_h);
        let rebuild = match &self.scaler {
            Some(s) => s.source() != src || s.destination() != dst,
            None => true,
        };
        if rebuild {
            self.scaler = Some(Scaler::new(src, dst).map_err(|e| e.to_string())?);
        }
        let sws = self
            .scaler
            .as_mut()
            .ok_or_else(|| "tile scaler unexpectedly absent".to_owned())?;
        let resized = sws.run(frame).map_err(|e| e.to_string())?;
        video_to_nv12(&resized, tag)
    }
}

/// Convert a libav NV12 [`Video`] frame into a CPU-reference [`Nv12Image`],
/// copying its (possibly stride-padded) planes into tightly-packed plane vecs.
fn video_to_nv12(frame: &Video, tag: mosaic_core::color::ColorInfo) -> Result<Nv12Image, String> {
    let w = frame.width();
    let h = frame.height();
    let wu = usize::try_from(w).map_err(|_| "width overflow".to_owned())?;
    let hu = usize::try_from(h).map_err(|_| "height overflow".to_owned())?;

    let mut y_plane = vec![0_u8; wu * hu];
    let mut uv_plane = vec![0_u8; wu * hu / 2];
    read_plane(&mut y_plane, frame.data(0), frame.stride(0), wu, hu)?;
    read_plane(&mut uv_plane, frame.data(1), frame.stride(1), wu, hu / 2)?;

    Nv12Image::new(w, h, y_plane, uv_plane, tag).map_err(|e| e.to_string())
}

/// Copy `rows` rows of `row_bytes` from a libav plane `src` (rows `src_stride`
/// apart) into a tightly-packed `dst`.
fn read_plane(
    dst: &mut [u8],
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<(), String> {
    if src_stride < row_bytes {
        return Err("libav plane stride is narrower than the row".to_owned());
    }
    for row in 0..rows {
        let src_start = row
            .checked_mul(src_stride)
            .ok_or_else(|| "src offset overflow".to_owned())?;
        let dst_start = row
            .checked_mul(row_bytes)
            .ok_or_else(|| "dst offset overflow".to_owned())?;
        let src_row = src
            .get(src_start..src_start + row_bytes)
            .ok_or_else(|| "src row out of range".to_owned())?;
        let dst_row = dst
            .get_mut(dst_start..dst_start + row_bytes)
            .ok_or_else(|| "dst row out of range".to_owned())?;
        dst_row.copy_from_slice(src_row);
    }
    Ok(())
}
