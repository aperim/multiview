//! The **real** libav\* end-to-end `mosaic run` pipeline (the `ffmpeg` feature).
//!
//! Where [`crate::run`] is the pure-software, FFmpeg-free smoke of invariant #1,
//! this module makes Mosaic *operable*: it ingests real video, composites it on
//! the CPU reference compositor driven by the engine's protected output core,
//! encodes the canvas **once**, and fans the encoded program out to the file and
//! HLS output sinks declared in the config.
//!
//! ```text
//! each source ─▶ own decode thread ─▶ scale to its cell ─▶ publish NV12
//!   (live or file/VOD, PTS-paced to wall-clock — invariant #4)        │
//!                                                                     ▼
//!                                                          per-tile TileStore
//!                                                          (last-good, lock-free)
//!                                                                     │  (sampled,
//!                                                                     │   never pacing)
//!  OutputClock ─▶ EngineRuntime drive (CPU compositor) ◀─────────────┘
//!       │
//!       └▶ one composited canvas per tick (out_pts = f(tick))
//!                                                              ├▶ encode ONCE (mpeg2video
//!                                                              │   by default; libx264 under
//!                                                              │   gpl-codecs)
//!                                                              └▶ fan out: FileSink + SegmentSink
//! ```
//!
//! ## Ingest is streamed, never buffered (BUG-2 fix)
//!
//! Each declared source decodes on its **own dedicated thread** and publishes
//! frames into its per-tile [`TileStore`](mosaic_framestore::TileStore) as they
//! arrive — the output clock starts immediately and **samples** the stores per
//! tick. A live stream (RTSP/HLS/SRT/RTMP/TS) never emits EOF, so the previous
//! "decode the whole source into a `Vec` before starting the clock" approach
//! hung forever and never honoured `--duration`/`--ticks`. Streaming makes a
//! live input a *sampled* producer that can neither pace nor stall the output
//! clock (invariant #1), and a bounded run (`run_for`) stops the clock after `N`
//! ticks and tears the ingest threads down (they cannot back-pressure the
//! engine — invariant #10).
//!
//! Per [invariant #4](../docs/research/streaming-gotchas.md) the ingest threads
//! pace each frame to wall-clock **by its PTS** (a custom pacer; `-re` is never
//! used) so a live source plays in real time and a file/VOD source plays at its
//! natural rate rather than being slurped as fast as the disk allows.
//!
//! ## Invariants upheld
//!
//! * **#1 output-clock.** The engine's [`EngineRuntime`](mosaic_engine::EngineRuntime)
//!   emits exactly one composited frame per tick; a source that has produced no
//!   frame yet, or has run out of frames, simply holds its last-good frame (or
//!   shows the slate) — it never stalls the loop. Ingest runs on separate
//!   threads the engine never waits on.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    /// Per-source native caption cue stores, keyed by source id. Each store is
    /// written by an isolated caption reader thread (HLS `WebVTT` rendition demux)
    /// and **sampled** at each output tick by the overlay baker, which burns the
    /// active cue into *that source's tile* (per-tile burn-in). A source with no
    /// caption selector (or whose rendition could not be resolved) is absent here
    /// and simply shows no caption. Native caption burn-in needs the `overlay`
    /// feature to render, so the stores are only built/sampled under it.
    #[cfg(feature = "overlay")]
    caption_stores: std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    /// Per-source caption reader plans (resolved at build time): the rendition
    /// `m3u8` to demux + the store to publish into. Consumed by the ingest
    /// supervisor (one reader thread each) on the first `drive`, like the video
    /// ingest plans. Empty ⇒ no native caption readers.
    #[cfg(feature = "overlay")]
    caption_plans: Vec<crate::captions::CaptionPlan>,
    /// An optional legacy `--subtitles` sidecar track, routed through the SAME
    /// per-tile burn-in path as native captions: its active cue is sampled per
    /// tick and burned into [`Self::sidecar_target`]'s tile. `None` ⇒ no sidecar.
    #[cfg(feature = "overlay")]
    subtitles: Option<mosaic_overlay::subtitle::CueTrack>,
    /// The source id the legacy `--subtitles` sidecar burns into (the first
    /// source-bound cell). `None` ⇒ no bound target (the sidecar shows nowhere).
    #[cfg(feature = "overlay")]
    sidecar_target: Option<String>,
    /// Per-source per-tick audio-loudness timelines (dBFS), keyed by source id,
    /// one inner entry per output tick, derived off the hot path from each
    /// source's own decoded audio (the per-input meter). A source absent from the
    /// map (a live URL, NDI, or an audio-free clip) rides its meter floor rather
    /// than showing a fabricated constant.
    #[cfg(feature = "overlay")]
    meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
    /// Per-source human label (`display_name`, or the id when unnamed), keyed by
    /// source id — the text drawn bottom-left of each tile.
    #[cfg(feature = "overlay")]
    tile_labels: std::collections::HashMap<String, String>,
    /// An optional **analog** clock face requested by a `[[overlays]]` entry with
    /// `kind = "clock"` + `face = "analog"`. `None` ⇒ only the default digital
    /// clock label is drawn.
    #[cfg(feature = "overlay")]
    analog_clock: Option<crate::overlays::AnalogClockSpec>,
    /// Per-source last-good-frame stores, keyed by source id. Shared (`Arc`)
    /// between the engine's drive loop (reader) and the ingest threads (writers).
    stores: HashMapStores,
    /// Per-source streaming ingest plans: how to open + decode each source, and
    /// the tile size its frames are scaled to. The drive starts one decode
    /// thread per plan; the threads publish into [`Self::stores`] as frames
    /// arrive (never buffered ahead of the clock — the BUG-2 fix).
    ingest_plans: Vec<IngestPlan>,
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

/// Everything one source needs to be ingested on its own decode thread: where
/// its media lives, the store to publish into, the tile size to scale to, the
/// canvas color tag, and whether it is a live (never-ending) source.
struct IngestPlan {
    /// The source id (for diagnostics / store keying).
    id: String,
    /// Where the media lives.
    location: SourceLocation,
    /// The destination tile size (even pixels) the frames are scaled to.
    tile_w: u32,
    /// The destination tile height.
    tile_h: u32,
    /// The store to publish decoded frames into (shared with the drive loop).
    store: Arc<TileStore<Nv12Image>>,
    /// Whether this is a live (continuous, never-EOF) source. Live sources are
    /// reopened on EOF/error (a transient HLS/RTSP drop reconnects); a finite
    /// file/VOD source plays once and then holds its last frame.
    live: bool,
    /// An optional **in-container DVB-sub route**: the muxed subtitle stream's
    /// index + time-base + the per-source cue store. When present, the video
    /// ingest loop decodes that stream's packets as a sibling of the video
    /// packets and publishes bitmap cues into the store (#36 Phase 2). `None` ⇒
    /// this source carries no native bitmap-caption decode. Only built under
    /// `overlay` (the burn-in renderer consumes the cues).
    #[cfg(feature = "overlay")]
    dvbsub: Option<DvbSubRoute>,
    /// A generated `test` clip's tempdir, kept alive for the life of the
    /// pipeline so the decode thread can open it.
    _owned: Option<GeneratedClip>,
}

/// The in-container DVB-sub decode route stashed on an [`IngestPlan`]: which
/// muxed subtitle stream to decode (index + its time-base) and the per-source
/// cue store the decoded bitmap cues are published into (shared with the baker).
#[cfg(feature = "overlay")]
struct DvbSubRoute {
    /// The subtitle stream index within the source container.
    stream_index: usize,
    /// The subtitle stream time-base (for the caption decoder's PTS rebase).
    time_base: Rational,
    /// The lock-free store the decoded bitmap cues are published into.
    store: Arc<crate::captions::CueStore>,
}

impl RealPipeline {
    /// Build the real pipeline from an already-validated configuration.
    ///
    /// Solves the layout, resolves each declared source to a streaming
    /// [`IngestPlan`] (it does **not** decode anything here — decoding happens on
    /// per-source threads started by [`RealPipeline::run_for`]/`run_until`, so a
    /// never-ending live source can never stall the build), resolves the output
    /// encoder (LGPL by default), and builds the runnable file/HLS sinks.
    ///
    /// # Errors
    ///
    /// Returns a [`PipelineError`] if the layout cannot be solved, a `test`
    /// source's clip cannot be generated, an NDI/unsupported source kind is
    /// declared, no runnable output is declared, or the encoder cannot be
    /// resolved.
    pub fn build(config: &MosaicConfig) -> Result<Self, PipelineError> {
        let layout = Arc::new(config.solve_layout()?);
        let cadence = config.canvas.fps.rational();
        let canvas_color = CanvasColor::default();
        let tag = canvas_color.output_tag();

        // Map each source id to the pixel size of the cell that binds it, so the
        // decoded frames are scaled to tile the canvas (the reference compositor
        // places tiles 1:1 at the cell origin).
        let mut stores: HashMapStores = std::collections::HashMap::new();
        let mut ingest_plans: Vec<IngestPlan> = Vec::with_capacity(config.sources.len());

        // Per-source native caption stores + reader plans. Built best-effort: a
        // source whose selector resolves to an HLS WebVTT rendition gets a store
        // (sampled per tile) and a reader plan (one isolated demux thread); a
        // resolve failure logs and yields no store (the tile shows no caption) —
        // it must never fail the build of a live source (invariant #1/#10). Only
        // built under `overlay` (native burn-in needs the renderer).
        #[cfg(feature = "overlay")]
        let mut caption_stores: std::collections::HashMap<
            String,
            Arc<crate::captions::CueStore>,
        > = std::collections::HashMap::new();
        #[cfg(feature = "overlay")]
        let mut caption_plans: Vec<crate::captions::CaptionPlan> = Vec::new();

        for source in &config.sources {
            let (tile_w, tile_h) = cell_pixel_size(&layout, &source.id)
                .unwrap_or((config.canvas.width, config.canvas.height));
            let store = Arc::new(TileStore::new(
                source.id.clone(),
                TileThresholds::default(),
                NoSignalPolicy::HoldForever,
            ));
            #[cfg_attr(not(feature = "overlay"), allow(unused_mut))]
            let mut plan = ingest_plan_for(source, tile_w, tile_h, Arc::clone(&store))?;

            // Wire this source's native captions (HLS WebVTT rendition thread +/or
            // in-container DVB-sub route), registering any cue store + reader plan
            // and stashing the dvbsub route on `plan`. Only under `overlay`.
            #[cfg(feature = "overlay")]
            wire_source_captions(source, &mut plan, &mut caption_stores, &mut caption_plans);

            stores.insert(source.id.clone(), store);
            ingest_plans.push(plan);
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

        // Derive the real PER-SOURCE per-tick audio-loudness timelines off the
        // build path: decode each file-backed source's OWN audio with
        // mosaic-audio's ballistics DSP and snapshot the meter at each tick, so a
        // tile's vertical meter reflects that input's own audio. A live URL /
        // NDI / audio-free source contributes no timeline (its tile meter then
        // rides its floor — no fabricated constant). This never touches the hot
        // path: it is computed once at build time.
        #[cfg(feature = "overlay")]
        let meter_db_timelines = build_meter_timelines(config, cadence);
        #[cfg(feature = "overlay")]
        let tile_labels = config
            .sources
            .iter()
            .map(|s| {
                let label = s.display_name.clone().unwrap_or_else(|| s.id.clone());
                (s.id.clone(), label)
            })
            .collect();
        // Read an optional analog clock face from a `[[overlays]]` clock entry.
        #[cfg(feature = "overlay")]
        let analog_clock =
            analog_clock_from_config(&config.overlays, config.canvas.width, config.canvas.height);

        // The legacy `--subtitles` sidecar (if attached later) burns into the
        // first source-bound cell. Pre-resolve that target id once here.
        #[cfg(feature = "overlay")]
        let sidecar_target = layout.cells.iter().find_map(|c| c.source.clone());

        Ok(Self {
            layout,
            cadence,
            stores,
            ingest_plans,
            #[cfg(feature = "overlay")]
            caption_stores,
            #[cfg(feature = "overlay")]
            caption_plans,
            canvas_color,
            nosignal_card,
            background: LinearRgba::opaque(0.02, 0.02, 0.05),
            encoder,
            outputs,
            #[cfg(feature = "overlay")]
            subtitles: None,
            #[cfg(feature = "overlay")]
            sidecar_target,
            #[cfg(feature = "overlay")]
            meter_db_timelines,
            #[cfg(feature = "overlay")]
            tile_labels,
            #[cfg(feature = "overlay")]
            analog_clock,
        })
    }

    /// Attach a parsed subtitle track whose active cue is burned into the
    /// program by the overlay baker (GAP-3). The track's cues are looked up by
    /// each output frame's media time, so the cue burns in exactly while it is
    /// active. Without the `overlay` feature this is a no-op identity.
    #[cfg(feature = "overlay")]
    #[must_use]
    pub fn with_subtitles(mut self, track: mosaic_overlay::subtitle::CueTrack) -> Self {
        self.subtitles = Some(track);
        self
    }

    /// Subtitle attachment is a no-op when the `overlay` feature is disabled
    /// (there is no overlay baker to burn the cue into); the track is dropped.
    #[cfg(not(feature = "overlay"))]
    #[must_use]
    pub fn with_subtitles(self, _track: mosaic_overlay::subtitle::CueTrack) -> Self {
        self
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
        let publisher: EnginePublisher<TickState, TickState> = EnginePublisher::new(EVENT_CAPACITY);

        // Start streaming ingest BEFORE the clock loop: one decode thread per
        // source, each publishing into its `TileStore` as frames arrive. The
        // supervisor owns the threads + a stop flag; it never blocks the engine
        // (the engine only ever *samples* the lock-free stores — invariant #10)
        // and it is torn down (stop + join) when this scope ends, so a bounded
        // run reliably stops the clock AND the ingest (invariant #1). Taking the
        // plans by value means a second `drive` call simply ingests nothing
        // (the stores hold their last frames / slate) rather than double-spawning.
        let plans = std::mem::take(&mut self.ingest_plans);
        // Native caption readers exist only under `overlay` (they feed the
        // per-tile burn-in renderer); without it there are none.
        #[cfg(feature = "overlay")]
        let caption_plans = std::mem::take(&mut self.caption_plans);
        #[cfg(not(feature = "overlay"))]
        let caption_plans: Vec<crate::captions::CaptionPlan> = Vec::new();
        let supervisor = IngestSupervisor::start(plans, caption_plans);

        // Prime the first frame per tile BEFORE constructing the runtime (whose
        // `new` seeds tick 0 to "now") and therefore before the output clock's
        // first tick (#40 startup-hold fix). See [`Self::prime_bound_tiles`] for
        // the bounded-wait rationale and the invariant-#1/#2 guarantees.
        self.prime_bound_tiles(ts.as_ref());

        // Build the runtime now (post-prime): `EngineRuntime::new` reads the seed
        // from `ts` here, so tick 0 is due at this instant — the prime delay sits
        // before the epoch and is never paid back as a burst.
        let mut runtime = EngineRuntime::new(clock, drive, ts, pacer);

        // Collected composited canvases (Arc to avoid copies), in tick order,
        // each paired with the per-source lifecycle states sampled that tick (so
        // the off-hot-path per-tile overlay flag reflects the real tile state).
        let collected: Arc<Mutex<Vec<CollectedTick>>> = Arc::new(Mutex::new(Vec::new()));

        // The projection runs once per tick on the hot loop. It snapshots the
        // composited canvas + per-source states into the collection — cheap,
        // panic-free, and never blocks the engine (the collection lock is held
        // only for a push; invariant #10 is not at risk because this is a bounded
        // offline collection, not a realtime consumer that could back-pressure a
        // live engine). Frame *advancement* is no longer done here: the ingest
        // threads publish into the stores asynchronously, and the drive loop
        // SAMPLES the latest-good frame per tick (inputs sampled, never pacing —
        // invariant #1).
        let collect = Arc::clone(&collected);
        // Snapshot the per-source active caption lines AT THIS TICK by sampling
        // each source's cue store at the frame's pts, on the hot loop alongside
        // the canvas. This is the correct point to sample a bounded drop-oldest
        // store: the off-hot-path bake runs only after the whole run, by which
        // time early cues would have been evicted from the small live window — so
        // we capture the cue active right now and stash it with this tick. The
        // sample is a pure lock-free read; it never paces or stalls the engine
        // (invariants #1/#10). Only under `overlay` (the burn-in renderer).
        #[cfg(feature = "overlay")]
        let caption_stores = self.caption_stores.clone();
        // The per-tile content-fault detector: shares (by `Arc`) the SAME
        // lock-free per-source last-good stores the engine samples, plus the
        // build-time per-source meter timeline (for silence). Each tick it
        // SAMPLES each tile's last-good luma + meter and folds black/freeze/
        // silence through per-source dwell/hysteresis. Sampling-only and
        // non-blocking: it can neither pace the output (inv #1) nor back-pressure
        // the engine (inv #10). Only built under `overlay` (the badge renderer).
        #[cfg(feature = "overlay")]
        let mut fault_detector = FaultDetector::new(
            self.stores.clone(),
            self.meter_db_timelines.clone(),
            self.cadence,
        );
        let state_of = move |frame: &CompositedFrame| -> TickState {
            #[cfg(feature = "overlay")]
            let captions = sample_caption_stores(&caption_stores, frame.pts());
            #[cfg(feature = "overlay")]
            let caption_bitmaps = sample_caption_bitmaps(&caption_stores, frame.pts());
            // Sample + classify each tile's content fault for THIS tick (a pure
            // lock-free read of the stores; fail-safe to no-fault on any error).
            #[cfg(feature = "overlay")]
            let faults = fault_detector.sample(frame.pts(), frame.tick.index, &frame.source_states);
            if let Ok(mut sink) = collect.lock() {
                sink.push(CollectedTick {
                    canvas: Arc::new(frame.canvas.clone()),
                    source_states: frame.source_states.clone(),
                    #[cfg(feature = "overlay")]
                    captions,
                    #[cfg(feature = "overlay")]
                    caption_bitmaps,
                    #[cfg(feature = "overlay")]
                    faults,
                });
            }
            TickState {
                tick: frame.tick.index,
                pts: frame.pts(),
            }
        };
        let event_of = |frame: &CompositedFrame| TickState {
            tick: frame.tick.index,
            pts: frame.pts(),
        };

        let outcome = match max_ticks {
            Some(max) => {
                runtime
                    .run_for(&publisher, stop, max, state_of, event_of)
                    .await
            }
            None => runtime.run(&publisher, stop, state_of, event_of).await,
        }
        .map_err(|e| PipelineError::Engine(e.to_string()));

        // The clock has stopped (bounded budget reached, or `stop` raised): tear
        // ingest down deterministically (signal + join) before reading the
        // collected frames. `drop(supervisor)` would also do this, but doing it
        // explicitly keeps the teardown ordering legible and lets a join error
        // surface in the log rather than being swallowed in a destructor.
        supervisor.shutdown();

        let outcome = outcome?;

        let ticks = match collected.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        let collected_count = u64::try_from(ticks.len()).unwrap_or(u64::MAX);
        let faltered = collected_count != outcome.ticks;

        // Bake configured per-tile overlays into the collected program OFF the hot
        // path (the protected output core has already emitted these frames). When
        // the `overlay` feature is off this is a no-op identity returning the bare
        // canvases.
        let frames = self.bake_overlays(ticks)?;

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

    /// Bounded **prime-wait** before the output clock's first tick (#40): hold
    /// the very first tick until every cell-bound source's [`TileStore`] has
    /// published one frame — so tick 0 samples real content, not the cold
    /// last-good / slate placeholder (the ~0.75 s "held first frame" startup
    /// transient). Called after the ingest threads are spawned and *before*
    /// [`EngineRuntime::new`] seeds tick 0, so the prime delay sits before the
    /// epoch and is never paid back as a catch-up burst.
    ///
    /// CRITICAL invariant #1/#2: the wait is hard-capped at [`PRIME_WAIT_BUDGET`]
    /// measured by `clock` (the same monotonic source the engine uses), so a
    /// source that never produces — the deliberately-missing source, a
    /// dead/wedged live input — can NOT block startup. Once the budget elapses
    /// the caller proceeds anyway and that tile rides its `NO_SIGNAL` / last-good
    /// placeholder (already produced by [`TileStore::read_at`]). It only delays
    /// the first tick; the cadence and per-tick logic are unchanged and no input
    /// ever paces the output. Bound sources only: an unbound source (no cell) is
    /// never sampled, so it must not be waited on.
    fn prime_bound_tiles(&self, clock: &dyn TimeSource) {
        let bound_ids: std::collections::HashSet<&str> = self
            .layout
            .cells
            .iter()
            .filter_map(|c| c.source.as_deref())
            .collect();
        let prime_stores: Vec<&Arc<TileStore<Nv12Image>>> = self
            .stores
            .iter()
            .filter(|(id, _)| bound_ids.contains(id.as_str()))
            .map(|(_, store)| store)
            .collect();
        let prime = wait_for_prime(
            &prime_stores,
            PRIME_WAIT_BUDGET,
            PRIME_WAIT_POLL,
            clock,
            std::thread::sleep,
        );
        if prime.all_primed {
            tracing::debug!(
                primed = prime.primed,
                total = prime.total,
                waited_ms = prime.waited.as_millis(),
                "all tiles primed before first output tick"
            );
        } else {
            tracing::warn!(
                primed = prime.primed,
                total = prime.total,
                waited_ms = prime.waited.as_millis(),
                "prime-wait budget elapsed with unprimed tile(s); starting clock anyway \
                 (they ride NO_SIGNAL/last-good — invariant #1)"
            );
        }
    }

    /// The loudness (dBFS) to show for source `id` at output tick `i`. Reads that
    /// source's own per-tick timeline derived at build time; falls back to the
    /// meter floor ([`mosaic_audio::Ballistics::FLOOR_DB`]) when the source has no
    /// decodable audio, so a silent / audio-free tile shows an empty bar rather
    /// than a fabricated constant.
    #[cfg(feature = "overlay")]
    fn meter_db_for(&self, id: &str, i: usize) -> f64 {
        match self.meter_db_timelines.get(id) {
            Some(timeline) => timeline
                .get(i)
                .copied()
                .or_else(|| timeline.last().copied())
                .unwrap_or(mosaic_audio::Ballistics::FLOOR_DB),
            None => mosaic_audio::Ballistics::FLOOR_DB,
        }
    }

    /// The legacy `--subtitles` **sidecar** caption lines active for source `id`
    /// at output instant `pts`, if `id` is the sidecar's target source
    /// ([`Self::sidecar_target`]).
    ///
    /// The sidecar track is fully in memory (no eviction), so it is sampled here
    /// in the off-hot-path bake rather than per-tick. Native HLS `WebVTT` cues
    /// are sampled per-tick on the hot loop instead (the cue store is a small
    /// live window), and merged ahead of this so both families share ONE per-tile
    /// burn-in path. A pure read; it never paces or stalls anything (#1/#10).
    #[cfg(feature = "overlay")]
    fn sidecar_caption_lines(&self, id: &str, pts: MediaTime) -> Option<Vec<String>> {
        if self.sidecar_target.as_deref() != Some(id) {
            return None;
        }
        let cue = self.subtitles.as_ref().and_then(|t| t.active_cue(pts))?;
        if cue.lines.is_empty() {
            None
        } else {
            Some(cue.lines.clone())
        }
    }

    /// Build the per-tile [`TileSpec`](crate::overlays::TileSpec) list from the
    /// solved layout's cells: one entry per source-bound cell, carrying the cell's
    /// pixel rectangle and the source's display label.
    #[cfg(feature = "overlay")]
    fn tile_specs(&self) -> Vec<crate::overlays::TileSpec> {
        use mosaic_overlay::geometry::PixelRect;
        let (cw, ch) = (self.layout.canvas.width, self.layout.canvas.height);
        let mut specs = Vec::new();
        for cell in &self.layout.cells {
            let Some(id) = cell.source.as_deref() else {
                continue;
            };
            let label = self
                .tile_labels
                .get(id)
                .cloned()
                .unwrap_or_else(|| id.to_owned());
            let rect = PixelRect {
                x: norm_to_px_f32(cell.x, cw),
                y: norm_to_px_f32(cell.y, ch),
                width: norm_to_px_f32(cell.w, cw),
                height: norm_to_px_f32(cell.h, ch),
            };
            specs.push(crate::overlays::TileSpec::new(id, label, rect));
        }
        specs
    }

    /// Bake the configured **per-tile** overlays into each collected program
    /// frame, off the hot path. With the `overlay` feature compiled in this
    /// rasterizes, for each layout cell, an input label + the source's own dB
    /// meter + a state/fault flag (plus the program clock + subtitle cue) and
    /// blends them into the NV12 frame via the compositor sub-pass; without it the
    /// bare canvases pass through unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`PipelineError::Engine`] if the overlay baker or sub-pass rejects
    /// the canvas (font load / unresolved color).
    // reason: the `not(feature)` sibling consumes `ticks` by value (identity
    // pass-through), so both arms share one by-value signature for the caller.
    #[cfg(feature = "overlay")]
    #[allow(clippy::needless_pass_by_value)]
    fn bake_overlays(
        &self,
        ticks: Vec<CollectedTick>,
    ) -> Result<Vec<Arc<Nv12Image>>, PipelineError> {
        use mosaic_compositor::overlay::apply_overlays_to_nv12;

        // Drive the on-screen clock from the REAL OS clock (CLOCK_REALTIME via
        // std; the host disciplines it via NTP). The displayed time-of-day is
        // sampled live at each bake (anti-drift) — it is a pure display concern
        // and never touches the engine's output cadence (invariant #1).
        let mut baker = crate::overlays::OverlayBaker::new(
            self.tile_specs(),
            crate::wallclock::WallClockSource::system(),
        )
        .map_err(|e| PipelineError::Engine(format!("overlay baker: {e}")))?;
        // Wire a configured analog clock face (the digital label stays on too).
        if let Some(spec) = self.analog_clock {
            baker = baker.with_analog_clock(spec);
        }

        let mut out_frames = Vec::with_capacity(ticks.len());
        for (i, tick) in ticks.iter().enumerate() {
            let pts = MediaTime::from_tick(i64::try_from(i).unwrap_or(i64::MAX), self.cadence);
            // Compose the per-tile dynamics for this output tick: each tile's OWN
            // decoded-audio loudness (its tile meter) + the real per-source
            // lifecycle state sampled by the engine that tick (its fault flag). A
            // source with no decodable audio rides the meter floor; a source not
            // present in the sampled states defaults to NO_SIGNAL inside the baker
            // (a missing source never stalls — invariant #1). The baker's per-tile
            // conflators thin the meter to ~30 Hz so the tap stays cheap and
            // cannot couple to the engine (invariant #10).
            let mut dynamics = std::collections::HashMap::new();
            // Per-source active caption lines at this output instant, sampled from
            // each source's cue store (native HLS WebVTT) plus the legacy sidecar
            // routed onto its target source — ONE per-tile burn-in path. Sampling
            // is a pure lock-free read; it never paces or stalls anything (#1/#10).
            // Native cues were sampled per-tick into `tick.captions` (the cue
            // store is a small live window); the in-memory sidecar track has no
            // eviction, so it is sampled here at `pts`. Native wins on overlap.
            let mut captions: std::collections::HashMap<String, Vec<String>> =
                tick.captions.clone();
            for spec in baker.tiles() {
                let state = tick
                    .source_states
                    .get(&spec.source_id)
                    .copied()
                    .unwrap_or(mosaic_core::traits::SourceState::NoSignal);
                // The content fault sampled + dwelled on the hot loop this tick;
                // a source absent from the map is healthy (no badge).
                let fault = tick
                    .faults
                    .get(&spec.source_id)
                    .copied()
                    .unwrap_or(crate::overlays::TileFault::None);
                dynamics.insert(
                    spec.source_id.clone(),
                    crate::overlays::TileDynamics {
                        meter_db: self.meter_db_for(&spec.source_id, i),
                        state,
                        fault,
                    },
                );
                if !captions.contains_key(&spec.source_id) {
                    if let Some(lines) = self.sidecar_caption_lines(&spec.source_id, pts) {
                        captions.insert(spec.source_id.clone(), lines);
                    }
                }
            }
            let list = baker
                .draw_list(pts, &dynamics, &captions, &tick.caption_bitmaps)
                .map_err(|e| PipelineError::Engine(format!("overlay draw: {e}")))?;
            let overlaid = apply_overlays_to_nv12(&tick.canvas, &list, self.canvas_color)
                .map_err(|e| PipelineError::Engine(format!("overlay blend: {e}")))?;
            out_frames.push(Arc::new(overlaid));
        }
        Ok(out_frames)
    }

    /// Overlays disabled at compile time: hand back the bare collected canvases.
    #[cfg(not(feature = "overlay"))]
    // reason: both arms share one `(&self, …) -> Result<…>` signature so the
    // caller stays feature-agnostic; this identity stub simply needs neither
    // `self` nor a fallible return.
    #[allow(clippy::unnecessary_wraps, clippy::unused_self)]
    fn bake_overlays(
        &self,
        ticks: Vec<CollectedTick>,
    ) -> Result<Vec<Arc<Nv12Image>>, PipelineError> {
        Ok(ticks.into_iter().map(|t| t.canvas).collect())
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

/// One collected output tick held for off-hot-path overlay baking: the bare
/// composited canvas plus the per-source lifecycle states sampled that tick (so
/// the per-tile fault flag reflects the real tile state, not a guess).
#[derive(Debug, Clone)]
struct CollectedTick {
    /// The composited canvas the protected output core emitted this tick.
    canvas: Arc<Nv12Image>,
    /// Per-source lifecycle state sampled this tick (`source_id -> state`); a
    /// cell with no bound source is omitted (and a source absent here is treated
    /// as `NO_SIGNAL` by the baker).
    // reason: only the `overlay`-on baker reads this (it drives the per-tile
    // fault flag); with overlays compiled out there is no flag to render, so the
    // field is legitimately unread in that build. The collector still samples it
    // unconditionally to keep one `CollectedTick` shape across features.
    #[cfg_attr(not(feature = "overlay"), allow(dead_code))]
    source_states: std::collections::HashMap<String, mosaic_core::traits::SourceState>,
    /// Per-source active caption lines sampled from the cue stores at THIS tick's
    /// pts (`source_id -> on-screen lines`). Captured on the hot loop because the
    /// bounded drop-oldest cue store only holds a small live window — sampling it
    /// once after the whole run would miss cues evicted meanwhile. A source with
    /// no active cue this tick is absent.
    #[cfg(feature = "overlay")]
    captions: std::collections::HashMap<String, Vec<String>>,
    /// Per-source active **bitmap** caption cue sampled from the cue stores at
    /// THIS tick's pts (`source_id -> CueBitmap`). Same hot-loop sampling
    /// rationale as `captions` (the bounded store holds a small live window); a
    /// pure lock-free read (#1/#10). A source with no active bitmap cue is absent.
    #[cfg(feature = "overlay")]
    caption_bitmaps: std::collections::HashMap<String, mosaic_ffmpeg::caption::CueBitmap>,
    /// Per-source content fault sampled this tick (`source_id -> fault`), folded
    /// through dwell/hysteresis by the [`FaultDetector`]. Captured on the hot
    /// loop because freeze needs the *previous* sampled frame and the dwell needs
    /// every tick in order — sampling once after the run would lose both. A
    /// healthy source maps to [`crate::overlays::TileFault::None`] (or is absent,
    /// which the baker treats as `None`).
    #[cfg(feature = "overlay")]
    faults: std::collections::HashMap<String, crate::overlays::TileFault>,
}

/// Sample every per-source caption cue store at `pts`, returning the active
/// caption lines per source (`source_id -> on-screen lines`). Only text cues
/// carry display lines; a source with no active cue at `pts` is omitted. Called
/// per tick on the hot loop — a pure lock-free read that can neither pace nor
/// stall the engine (invariants #1/#10).
#[cfg(feature = "overlay")]
fn sample_caption_stores(
    stores: &std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    pts: MediaTime,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    for (id, store) in stores {
        if let Some(mosaic_ffmpeg::caption::CaptionCue::Text { text, .. }) = store.active_at(pts) {
            if !text.lines.is_empty() {
                out.insert(id.clone(), text.lines);
            }
        }
    }
    out
}

/// Sample every per-source caption cue store at `pts`, returning the active
/// **bitmap** cue per source (`source_id -> CueBitmap`). The sibling of
/// [`sample_caption_stores`] for the [`CaptionCue::Bitmap`] shape (DVB-sub); a
/// source with no active bitmap cue at `pts` is omitted. Called per tick on the
/// hot loop — a pure lock-free read that can neither pace nor stall the engine
/// (invariants #1/#10).
#[cfg(feature = "overlay")]
fn sample_caption_bitmaps(
    stores: &std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    pts: MediaTime,
) -> std::collections::HashMap<String, mosaic_ffmpeg::caption::CueBitmap> {
    let mut out = std::collections::HashMap::new();
    for (id, store) in stores {
        if let Some(mosaic_ffmpeg::caption::CaptionCue::Bitmap { bitmap, .. }) =
            store.active_at(pts)
        {
            out.insert(id.clone(), bitmap);
        }
    }
    out
}

/// The dBFS floor at/below which the per-input meter is treated as **silent**
/// for the audio-loss fault. Just above the meter's true floor so a genuinely
/// quiet-but-present programme does not trip it; sustained past the silence
/// dwell before the `NO AUDIO` badge raises (anti-flap). A source with no
/// build-time meter timeline rides [`mosaic_audio::Ballistics::FLOOR_DB`], which
/// is below this floor, so an audio-free tile reads silent (intended).
#[cfg(feature = "overlay")]
const SILENCE_FLOOR_DB: f64 = -50.0;

/// Samples each tile's last-good frame + per-input loudness once per output tick
/// and classifies a per-tile **content fault** (black / frozen / silent),
/// distinct from the lifecycle [`SourceState`](mosaic_core::traits::SourceState).
///
/// It shares the SAME lock-free per-source [`TileStore`]s the engine samples (by
/// `Arc`), so it never copies the picture and never blocks: a [`TileStore::read_at`]
/// is a wait-free atomic snapshot. Black/freeze come from the stateless engine
/// probes ([`mosaic_engine::BlackProbe`]/[`mosaic_engine::FreezeProbe`]) run over
/// a borrowed [`mosaic_engine::LumaView`] of the sampled frame's tightly-packed
/// luma plane; freeze compares the current sample to the *previous* sampled frame
/// (held by `Arc`, no copy). Silence comes from the build-time per-source meter
/// timeline. Each instantaneous condition is folded through a per-source
/// [`mosaic_engine::AlarmStateMachine`] so the badge dwells/hysteresis rather than
/// flapping. Any probe/geometry error logs and yields *no fault* (fail-safe):
/// fault detection must never break the output clock (inv #1) or the engine (#10).
///
/// ## Freeze on a REAL encoded source (not just byte-identical frames)
///
/// A genuinely frozen feed delivered as compressed video is **not** byte-identical
/// frame-to-frame: at every GOP boundary the codec re-quantizes the (unchanging)
/// picture, perturbing a small fraction of luma samples. Instrumenting the demo's
/// `frozen.ts` (mpeg2, ~1 s GOP) through this exact decode→scale→NV12 path showed
/// the per-frame changed-fraction sits at `0.0` for most frames but **spikes to
/// 1.2 %–4.6 % once per GOP** at the default tolerance (`diff_tolerance = 2`). The
/// engine [`FreezeConfig::default`] threshold is `0.1 %`, so each GOP spike reads
/// "not frozen" and, fed straight to the 2 s dwell machine, *resets the dwell*
/// every ~1 s — the freeze alarm never sustains long enough to raise.
///
/// We must not change the shared engine probe defaults (the alarm system depends
/// on them), so the CLI configures **its** freeze probe for real-world codec
/// noise (see [`Self::new`]): raising `diff_tolerance` to `6` collapses the GOP
/// spikes (the same source then sees max `1.27 %`, with `148 / 150` frames at or
/// below the engine-default `0.1 %` change threshold, which is therefore KEPT —
/// it cleanly separates a frozen feed from a moving-but-silent one, which stays
/// `> 0.1 %` on `146 / 150` frames). On top of that, the instantaneous freeze
/// condition is **debounced** over a short sliding window
/// ([`FREEZE_DEBOUNCE_WINDOW`]/[`FREEZE_DEBOUNCE_MIN_PRESENT`]) before it reaches
/// the dwell machine, so the *one or two* residual per-GOP noisy frames (and the
/// first decoded frame) cannot reset an otherwise-sustained freeze. Black and
/// silence are unaffected.
#[cfg(feature = "overlay")]
struct FaultDetector {
    /// The per-source last-good stores, shared with the engine drive loop.
    stores: HashMapStores,
    /// Build-time per-source per-tick loudness timelines (dBFS) for silence.
    meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
    /// Per-source dwell/hysteresis state machines for each fault class.
    machines: std::collections::HashMap<String, SourceFaultMachines>,
    /// The stateless black probe (default broadcast threshold).
    black: mosaic_engine::BlackProbe,
    /// The stateless freeze probe, tuned for real encoded sources (see
    /// [`Self::new`]): wider per-sample tolerance + change threshold than the
    /// engine default so GOP re-quantization noise does not read as motion.
    freeze: mosaic_engine::FreezeProbe,
    /// The previous sampled frame per source, for the freeze comparison (held by
    /// `Arc`, so caching it copies no pixels).
    previous: std::collections::HashMap<String, Arc<Nv12Image>>,
    /// Per-source sliding window of the most recent instantaneous freeze
    /// conditions (newest pushed back, oldest popped front), used to debounce a
    /// single noisy frame so it cannot reset the freeze dwell.
    freeze_window: std::collections::HashMap<String, std::collections::VecDeque<bool>>,
    /// Dwell-up/dwell-down windows (derived from the cadence) per fault class.
    hysteresis_black: mosaic_engine::AlarmHysteresis,
    hysteresis_freeze: mosaic_engine::AlarmHysteresis,
    hysteresis_silence: mosaic_engine::AlarmHysteresis,
}

/// The number of recent sampled frames over which the instantaneous freeze
/// condition is debounced before it reaches the dwell machine (~0.5 s at 25 fps).
/// Sized so it spans more than one GOP boundary, so an isolated per-GOP noise
/// spike is outvoted by the surrounding frozen frames.
#[cfg(feature = "overlay")]
const FREEZE_DEBOUNCE_WINDOW: usize = 12;

/// How many of the [`FREEZE_DEBOUNCE_WINDOW`] most-recent frames must read frozen
/// for the debounced freeze condition to be "present". At `9 / 12` (75 %) a single
/// (or even a couple of) noisy GOP-boundary frame(s) inside the window cannot
/// flip the condition to absent and reset the dwell, while a genuinely moving
/// picture (mostly-changed frames) stays well below the bar.
#[cfg(feature = "overlay")]
const FREEZE_DEBOUNCE_MIN_PRESENT: usize = 9;

/// The three per-source dwell state machines (black / freeze / silence).
#[cfg(feature = "overlay")]
struct SourceFaultMachines {
    black: mosaic_engine::AlarmStateMachine,
    freeze: mosaic_engine::AlarmStateMachine,
    silence: mosaic_engine::AlarmStateMachine,
}

#[cfg(feature = "overlay")]
impl FaultDetector {
    /// Build a detector over the shared `stores` + build-time meter `timelines`.
    /// The dwell windows are absolute media durations (cadence-agnostic), so the
    /// output `cadence` is taken only to make the timeline these dwells run on
    /// explicit at the call site.
    fn new(
        stores: HashMapStores,
        meter_db_timelines: std::collections::HashMap<String, Vec<f64>>,
        _cadence: Rational,
    ) -> Self {
        use mosaic_engine::{AlarmHysteresis, BlackConfig, BlackProbe, FreezeConfig, FreezeProbe};
        // Dwell windows on the media timeline. Black/silence raise after ~0.5 s of
        // the condition and clear after ~0.3 s of its absence; freeze needs a
        // longer ~2 s of identical frames so a brief genuine still does not trip
        // it. These give the anti-flap hysteresis without coupling to wall-clock.
        let dwell = |secs_num: i64, secs_den: i64| -> MediaTime {
            MediaTime::from_nanos(secs_num.saturating_mul(1_000_000_000) / secs_den.max(1))
        };
        let down = dwell(3, 10); // 0.3 s
                                 // The CLI freeze probe widens only the PER-SAMPLE tolerance vs the engine
                                 // default — the root cause of the missed badge was tolerance, not the
                                 // change threshold. Instrumented across the three real demo sources scaled
                                 // to a 636x356 tile, the changed-fraction with diff_tolerance=6 was:
                                 //   frozen:  median 0.000%, max 1.27%, 148/150 frames <= 0.1%
                                 //   silent:  median 0.229%, max 1.91%,   4/150 frames <= 0.1%  (moving)
                                 //   healthy: median 3.13%,  max 71%,      5/150 frames <= 0.1%
                                 // diff_tolerance=6 collapses GOP re-quantization noise (the frozen source's
                                 // per-GOP spike drops from 1.2-4.6% at the default tolerance 2 to <= 1.27%,
                                 // its steady frames to 0.000%); the engine-default change_threshold (0.1%)
                                 // is KEPT because at tolerance 6 it already separates a frozen feed (148/150
                                 // below) from a moving-but-silent feed (only 4/150 below). An earlier 0.5%
                                 // threshold over-loosened this and wrongly flagged the moving silent tile.
                                 // The two residual per-GOP spikes the frozen source still shows are absorbed
                                 // by the debounce window below, not by a looser threshold.
        let freeze_cfg = FreezeConfig::default().with_tolerance(6);
        Self {
            stores,
            meter_db_timelines,
            machines: std::collections::HashMap::new(),
            black: BlackProbe::new(BlackConfig::default()),
            freeze: FreezeProbe::new(freeze_cfg),
            previous: std::collections::HashMap::new(),
            freeze_window: std::collections::HashMap::new(),
            hysteresis_black: AlarmHysteresis::new(dwell(1, 2), down), // 0.5 s up
            hysteresis_freeze: AlarmHysteresis::new(dwell(2, 1), down), // 2 s up
            hysteresis_silence: AlarmHysteresis::new(dwell(1, 2), down), // 0.5 s up
        }
    }

    /// Get-or-create the dwell machines for `id` (one per fault class).
    fn machines_for(&mut self, id: &str) -> &mut SourceFaultMachines {
        use mosaic_core::alarm::{AlarmId, AlarmKind, AlarmScope, PerceivedSeverity};
        use mosaic_engine::{AlarmHysteresis, AlarmStateMachine};
        let hb = self.hysteresis_black;
        let hf = self.hysteresis_freeze;
        let hs = self.hysteresis_silence;
        self.machines.entry(id.to_owned()).or_insert_with(|| {
            let mk = |kind: AlarmKind, sev: PerceivedSeverity, hyst: AlarmHysteresis| {
                AlarmStateMachine::new(
                    AlarmId::new(format!("{id}:{kind:?}")),
                    kind,
                    AlarmScope::Probe { id: id.to_owned() },
                    sev,
                    hyst,
                )
            };
            SourceFaultMachines {
                black: mk(AlarmKind::Black, PerceivedSeverity::Major, hb),
                freeze: mk(AlarmKind::Freeze, PerceivedSeverity::Major, hf),
                silence: mk(AlarmKind::Silence, PerceivedSeverity::Minor, hs),
            }
        })
    }

    /// Sample + classify every cell-bound source's content fault for the output
    /// instant `pts` (tick `index`), returning the active fault per source.
    ///
    /// `source_states` names the cell-bound sources this tick. For each, this
    /// reads its last-good frame (lock-free), runs the black + freeze probes over
    /// its luma, reads its silence condition from the build-time meter timeline,
    /// folds each through the per-source dwell machine, and returns the
    /// highest-precedence active fault (black > freeze > silence). A source with
    /// no usable frame (`NoSignal`) contributes no content fault (its lifecycle
    /// badge already conveys the loss). Errors are logged and treated as no-fault.
    fn sample(
        &mut self,
        pts: MediaTime,
        index: u64,
        source_states: &std::collections::HashMap<String, mosaic_core::traits::SourceState>,
    ) -> std::collections::HashMap<String, crate::overlays::TileFault> {
        use crate::overlays::TileFault;
        let mut out = std::collections::HashMap::new();
        // Snapshot the source ids this tick (sorted for deterministic logging).
        let mut ids: Vec<String> = source_states.keys().cloned().collect();
        ids.sort_unstable();
        for id in ids {
            // Sample this tile's last-good frame (lock-free; never blocks).
            let frame = self
                .stores
                .get(&id)
                .and_then(|store| store.read_at(pts).frame().map(Arc::clone));

            // Instantaneous black / freeze conditions from the sampled luma.
            let (black_now, freeze_now) = match &frame {
                Some(img) => self.picture_conditions(&id, img),
                None => (false, false),
            };
            // Update the previous-frame cache for the next freeze comparison.
            // On a missing frame (NoSignal) drop both the previous frame AND the
            // debounce window so a recovered source restarts freeze cleanly rather
            // than inheriting stale frozen votes across the gap.
            if let Some(img) = &frame {
                self.previous.insert(id.clone(), Arc::clone(img));
            } else {
                self.previous.remove(&id);
                self.freeze_window.remove(&id);
            }
            // Instantaneous silence from the per-input meter timeline.
            let silence_now = self.silence_now(&id, index);

            // Debounce the freeze condition over a short window so a single noisy
            // GOP-boundary / warm-up frame cannot reset the 2 s freeze dwell.
            let freeze_debounced = self.debounce_freeze(&id, freeze_now);

            // Fold each condition through its per-source dwell machine.
            let machines = self.machines_for(&id);
            machines.black.observe(black_now, pts);
            machines.freeze.observe(freeze_debounced, pts);
            machines.silence.observe(silence_now, pts);

            // Precedence: black > freeze > silence (a black picture is the most
            // specific/severe content fault; a still or silent tile is lesser).
            let fault = if machines.black.is_active() {
                TileFault::Black
            } else if machines.freeze.is_active() {
                TileFault::Frozen
            } else if machines.silence.is_active() {
                TileFault::Silent
            } else {
                TileFault::None
            };
            if fault.is_present() {
                out.insert(id, fault);
            }
        }
        out
    }

    /// The instantaneous (black, frozen) conditions for `id`'s sampled `frame`,
    /// over a borrowed luma view of its tightly-packed Y plane. Any geometry
    /// error logs and yields `(false, false)` (fail-safe to no fault).
    fn picture_conditions(&self, id: &str, frame: &Nv12Image) -> (bool, bool) {
        use mosaic_engine::LumaView;
        // The Y plane is tightly packed (`stride == width`) per `Nv12Image`.
        let current = match LumaView::packed(frame.y_plane(), frame.width(), frame.height()) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(source = %id, error = %e, "fault probe: luma view build failed");
                return (false, false);
            }
        };
        let black = self.black.detect(&current).condition_present;
        // Freeze needs the previous sampled frame; if none yet (first frame or a
        // gap), it is not frozen this tick (fail-safe toward "live").
        let frozen = match self.previous.get(id) {
            Some(prev) => match LumaView::packed(prev.y_plane(), prev.width(), prev.height()) {
                Ok(prev_view) => self.freeze.detect(&current, &prev_view).condition_present,
                Err(_) => false,
            },
            None => false,
        };
        (black, frozen)
    }

    /// Push this tick's instantaneous freeze condition into `id`'s sliding window
    /// and return the **debounced** condition: frozen-now iff at least
    /// [`FREEZE_DEBOUNCE_MIN_PRESENT`] of the last [`FREEZE_DEBOUNCE_WINDOW`]
    /// frames read frozen.
    ///
    /// This is the anti-flap layer between the per-frame probe and the dwell
    /// machine: the engine's [`AlarmStateMachine`](mosaic_engine::AlarmStateMachine)
    /// resets its raise dwell on a *single* absent sample (correct for its general
    /// use, and not ours to change), but a real frozen feed emits an occasional
    /// noisy frame at each GOP boundary. Requiring a strong majority of the recent
    /// window to be frozen lets those isolated spikes pass without resetting the
    /// dwell, while a genuinely moving picture (mostly-changed frames) never
    /// reaches the majority and so never debounces to frozen. Until the window
    /// has filled it reports the simple majority of what it has seen so far.
    fn debounce_freeze(&mut self, id: &str, freeze_now: bool) -> bool {
        let window = self.freeze_window.entry(id.to_owned()).or_default();
        window.push_back(freeze_now);
        while window.len() > FREEZE_DEBOUNCE_WINDOW {
            window.pop_front();
        }
        let present = window.iter().filter(|&&f| f).count();
        if window.len() >= FREEZE_DEBOUNCE_WINDOW {
            // Full window: require the strong majority threshold.
            present >= FREEZE_DEBOUNCE_MIN_PRESENT
        } else {
            // Warming up: a simple majority of the frames seen so far. A genuinely
            // frozen source reads frozen from frame 2 onward; a moving one does not.
            present.saturating_mul(2) > window.len()
        }
    }

    /// The instantaneous silence condition for `id` at tick `index`: the source's
    /// build-time meter reading is at/below [`SILENCE_FLOOR_DB`]. A source with no
    /// meter timeline rides the meter floor (which is below the silence floor), so
    /// an audio-free tile reads silent.
    fn silence_now(&self, id: &str, index: u64) -> bool {
        let db = match self.meter_db_timelines.get(id) {
            Some(timeline) => {
                let i = usize::try_from(index).unwrap_or(usize::MAX);
                timeline
                    .get(i)
                    .copied()
                    .or_else(|| timeline.last().copied())
                    .unwrap_or(mosaic_audio::Ballistics::FLOOR_DB)
            }
            None => mosaic_audio::Ballistics::FLOOR_DB,
        };
        db <= SILENCE_FLOOR_DB
    }
}

/// Owns the per-source streaming-ingest decode threads and a shared stop flag.
///
/// Each thread decodes one source and publishes frames into its [`TileStore`]
/// (shared lock-free with the engine's drive loop). The engine only ever
/// *samples* those stores, so a slow, fast, or never-ending ingest thread can
/// neither pace nor stall the output clock (invariant #1) and cannot
/// back-pressure the engine (invariant #10). [`IngestSupervisor::shutdown`]
/// raises the stop flag and joins every thread, so a bounded run tears ingest
/// down deterministically rather than leaking threads.
struct IngestSupervisor {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl IngestSupervisor {
    /// Spawn one decode thread per video plan **and** one caption reader thread
    /// per caption plan, then return the running supervisor.
    ///
    /// A caption reader is just another best-effort writer of a lock-free store
    /// (the cue store) — it shares the same stop flag and is joined the same way,
    /// so it can neither pace nor stall the output clock (invariant #1) nor
    /// back-pressure the engine (invariant #10).
    fn start(plans: Vec<IngestPlan>, caption_plans: Vec<crate::captions::CaptionPlan>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(plans.len().saturating_add(caption_plans.len()));
        for plan in plans {
            let stop = Arc::clone(&stop);
            let id = plan.id.clone();
            let builder = std::thread::Builder::new().name(format!("mosaic-ingest-{id}"));
            match builder.spawn(move || ingest_loop(&plan, &stop)) {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    // A thread that cannot spawn is logged and skipped: its tile
                    // simply rides NO_SIGNAL (slate) rather than failing the run
                    // (invariant #1 — the output clock is independent of inputs).
                    tracing::error!(error = %e, source = %id, "could not spawn ingest thread");
                }
            }
        }
        for plan in caption_plans {
            let stop = Arc::clone(&stop);
            let id = plan.id.clone();
            let builder = std::thread::Builder::new().name(format!("mosaic-captions-{id}"));
            match builder.spawn(move || crate::captions::caption_loop(&plan, &stop)) {
                Ok(handle) => handles.push(handle),
                Err(e) => {
                    // A caption reader that cannot spawn is logged and skipped:
                    // its tile simply shows no caption (best-effort — invariant
                    // #1; captions never gate the output clock).
                    tracing::error!(error = %e, source = %id, "could not spawn caption reader thread");
                }
            }
        }
        Self { stop, handles }
    }

    /// Signal every ingest thread to stop and join them.
    fn shutdown(mut self) {
        self.join_all();
    }

    /// Raise the stop flag, then join every outstanding ingest thread within a
    /// bounded grace period.
    ///
    /// A thread blocked inside a libav **network** call (`ffmpeg::format::input`
    /// opening a stalled live URL, or a `packet.read` on a wedged socket) cannot
    /// observe the cooperative `stop` flag — libav offers no portable cancel
    /// from a safe wrapper. Blocking forever on `join` would defeat the whole
    /// BUG-2 fix (the bounded run would hang on teardown), so a thread that does
    /// not finish within [`INGEST_JOIN_GRACE`] is **detached**: it only ever
    /// *writes* a lock-free store it shares by `Arc`, owns its own libav state
    /// (freed in `Drop`), and is reaped at process exit — it can neither corrupt
    /// the produced output nor stall the caller. This keeps the output-clock
    /// guarantee (invariant #1) intact end-to-end, including teardown.
    fn join_all(&mut self) {
        self.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + INGEST_JOIN_GRACE;
        for handle in self.handles.drain(..) {
            let name = handle.thread().name().unwrap_or("ingest").to_owned();
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !handle.is_finished() {
                tracing::warn!(
                    thread = %name,
                    "ingest thread wedged in a blocking libav call; detaching (reaped at exit)"
                );
                continue; // detach: drop the handle without joining.
            }
            if handle.join().is_err() {
                tracing::error!(thread = %name, "ingest thread panicked during join");
            }
        }
    }
}

impl Drop for IngestSupervisor {
    fn drop(&mut self) {
        // Defensive teardown if `shutdown` was not called (e.g. an early return
        // on the encode/error path): raise the flag and join (bounded) so no
        // thread blocks the caller. After `shutdown` the handle vec is already
        // drained, so this is a no-op on that path.
        self.join_all();
    }
}

/// The outcome of the startup prime-wait: did every store prime, and how long
/// the wait actually took. Returned for diagnostics/logging and asserted by the
/// deterministic prime tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrimeOutcome {
    /// How many of `total` stores were primed when the wait returned.
    primed: usize,
    /// The total number of bound stores waited on.
    total: usize,
    /// The wall-time the wait actually spent (clock-measured).
    waited: Duration,
    /// `true` if the wait returned because every store primed; `false` if it
    /// returned because [`PRIME_WAIT_BUDGET`] elapsed first (a dead/slow source).
    all_primed: bool,
}

/// Wait — **bounded** — for every store in `stores` to publish its first frame,
/// so the output clock's very first tick samples primed tiles instead of the
/// cold last-good placeholder (the #40 startup-hold fix).
///
/// The wait is the heart of why this is safe for invariant #1/#2: it polls
/// `is_primed` every `poll` and returns the instant all stores are primed, but
/// it is hard-capped at `budget` measured by the injected `clock`. A source that
/// never produces a frame (the deliberately-missing source, a dead/wedged live
/// input) therefore can NOT block startup — once `budget` elapses the wait
/// returns with `all_primed = false` and the caller starts the clock anyway; the
/// unprimed tiles ride their `NO_SIGNAL` / last-good placeholder, which
/// [`TileStore::read_at`] already produces. It never paces the engine and never
/// touches the cadence — it only delays the first tick by at most `budget`.
///
/// `clock` + `sleep` are injected so the whole wait is deterministically
/// testable with no real sleeping: a test passes a [`ManualTimeSource`] and a
/// `sleep` closure that advances it, so the budget/poll behaviour is exercised
/// without wall-clock flakiness. Production passes a real monotonic clock and
/// `std::thread::sleep`.
fn wait_for_prime<S>(
    stores: &[&Arc<TileStore<Nv12Image>>],
    budget: Duration,
    poll: Duration,
    clock: &dyn TimeSource,
    mut sleep: S,
) -> PrimeOutcome
where
    S: FnMut(Duration),
{
    let total = stores.len();
    let count_primed = |stores: &[&Arc<TileStore<Nv12Image>>]| -> usize {
        stores.iter().filter(|s| s.is_primed()).count()
    };

    let start = clock.now_nanos();
    let budget_ns = i64::try_from(budget.as_nanos()).unwrap_or(i64::MAX);

    // No bound stores ⇒ nothing to prime; do not delay the clock at all.
    if total == 0 {
        return PrimeOutcome {
            primed: 0,
            total: 0,
            waited: Duration::ZERO,
            all_primed: true,
        };
    }

    loop {
        let primed = count_primed(stores);
        let elapsed_ns = clock.now_nanos().saturating_sub(start).max(0);
        if primed == total {
            // Every input primed: start the clock immediately (typically well
            // inside the budget).
            return PrimeOutcome {
                primed,
                total,
                waited: duration_from_nanos(elapsed_ns),
                all_primed: true,
            };
        }
        if elapsed_ns >= budget_ns {
            // Budget spent: a source has not primed and we must NOT wait on it
            // (invariant #1). Start the clock; its tile rides its placeholder.
            return PrimeOutcome {
                primed,
                total,
                waited: duration_from_nanos(elapsed_ns),
                all_primed: false,
            };
        }
        // Sleep at most the remaining budget so the last poll lands on the cap.
        let remaining = budget_ns.saturating_sub(elapsed_ns);
        let nap = poll.min(duration_from_nanos(remaining));
        sleep(nap);
    }
}

/// Build a [`Duration`] from a non-negative nanosecond count, saturating a
/// negative input to zero (the guardrails deny `as` casts; this stays lossless).
fn duration_from_nanos(nanos: i64) -> Duration {
    Duration::from_nanos(u64::try_from(nanos.max(0)).unwrap_or(u64::MAX))
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

/// Convert a normalized fraction in `[0,1]` to a pixel coordinate `f32` against
/// `extent`, `as`-cast-free, for placing the per-tile overlay surface. Reuses the
/// exact integer [`frac_to_px`] then widens losslessly (overlay sizes are well
/// under 2^24).
#[cfg(feature = "overlay")]
fn norm_to_px_f32(frac: f32, extent: u32) -> f32 {
    let px = frac_to_px(frac, extent);
    let high = u16::try_from(px >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(px & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Whole ticks in one second at `cadence` (`num/den`), rounded to nearest, used
/// as the GOP / segment length. Exact integer arithmetic (never float fps).
fn ticks_per_second(cadence: Rational) -> u32 {
    let num = i128::from(cadence.num);
    let den = i128::from(cadence.den).max(1);
    let rounded = (num + den / 2) / den;
    u32::try_from(rounded.max(1)).unwrap_or(u32::MAX)
}

/// Build the real **per-source** per-tick audio-loudness timelines (dBFS) off the
/// build path — one entry per **file-backed** source that decodes to audio.
///
/// For each `file`/`test` source it runs that source's own decoded 48 kHz
/// samples through a sample-peak [`mosaic_audio::Ballistics`] meter and snapshots
/// the meter at each output-tick boundary, producing one dBFS value per tick. The
/// meter is the program-loudness DSP, so a silent track reads its floor and a
/// loud track reads high — each tile's on-screen bar reflects *that input's* own
/// audio.
///
/// A source with no decodable audio is simply omitted from the map (live URLs are
/// not pre-decoded here — they never EOF; NDI/unknown carry no file; an
/// audio-free clip yields nothing); its tile then rides the meter floor rather
/// than fabricating a constant. This runs once at build time and never touches
/// the hot path (invariant #1/#10).
#[cfg(feature = "overlay")]
fn build_meter_timelines(
    config: &MosaicConfig,
    cadence: Rational,
) -> std::collections::HashMap<String, Vec<f64>> {
    let mut timelines = std::collections::HashMap::new();
    for source in &config.sources {
        // Resolve a decodable local path, keeping any generated `test` clip's
        // tempdir alive (`_clip`) for the whole decode.
        let (path, _clip): (PathBuf, Option<GeneratedClip>) = match &source.kind {
            SourceKind::File { path } => (PathBuf::from(path), None),
            SourceKind::Test => match generate_test_clip(&source.id) {
                Ok(clip) => (clip.0.clone(), Some(clip)),
                Err(_) => continue,
            },
            // Live URLs are not pre-decoded here (they never EOF); NDI/unknown
            // carry no file. They contribute no build-time meter timeline.
            _ => continue,
        };
        match meter_timeline_for_file(&path, cadence) {
            Ok(timeline) if !timeline.is_empty() => {
                timelines.insert(source.id.clone(), timeline);
            }
            Ok(_) => {}
            Err(reason) => {
                tracing::debug!(source = %source.id, %reason, "no per-input meter timeline");
            }
        }
    }
    timelines
}

/// Decode `path`'s audio and snapshot a sample-peak meter at each output-tick
/// boundary, yielding one dBFS reading per tick.
#[cfg(feature = "overlay")]
fn meter_timeline_for_file(path: &Path, cadence: Rational) -> Result<Vec<f64>, String> {
    use mosaic_audio::decode::AudioFileDecoder;
    use mosaic_audio::{Ballistics, ChannelLayout, MeterScale, PeakMode};

    let mut decoder =
        AudioFileDecoder::open(path, ChannelLayout::Stereo).map_err(|e| e.to_string())?;
    let format = decoder.format();
    let rate = format.sample_rate();
    let channels = format.channel_count().max(1);
    let mut meter = Ballistics::new(rate, MeterScale::SamplePeak(PeakMode::Sample));

    // Samples per output tick = sample_rate * den / num (exact integer; never
    // float fps). At least one sample per tick so a tick always advances.
    let num = i128::from(cadence.num).max(1);
    let den = i128::from(cadence.den).max(1);
    let samples_per_tick = (i128::from(rate).saturating_mul(den) / num).max(1);

    let mut timeline = Vec::new();
    // Drive the meter sample-by-sample (downmix to mono by averaging the frame's
    // channels — the meter wants a single program-loudness reading), emitting the
    // reading each time we cross a tick boundary in input samples.
    let mut samples_since_tick: i128 = 0;
    while let Some(block) = decoder.next_block().map_err(|e| e.to_string())? {
        for frame in block.interleaved().chunks_exact(channels) {
            let sum: f32 = frame.iter().copied().sum();
            let mono = f64::from(sum) / f64::from(u32_from_usize_audio(channels));
            meter.push(mono);
            samples_since_tick = samples_since_tick.saturating_add(1);
            if samples_since_tick >= samples_per_tick {
                timeline.push(meter.reading_db());
                samples_since_tick = 0;
            }
        }
    }
    // Flush a final partial tick so a short clip still contributes a reading.
    if samples_since_tick > 0 {
        timeline.push(meter.reading_db());
    }
    Ok(timeline)
}

/// Saturating `usize` → `u32` for the audio-channel divisor (no `as`).
#[cfg(feature = "overlay")]
fn u32_from_usize_audio(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// The codec token a config output names, if it carries one.
/// Read an optional analog clock face from the config `[[overlays]]` list: the
/// first entry whose `kind == "clock"` and whose `face` param is `"analog"`.
///
/// Placement comes from optional `x`/`y`/`radius` params (canvas pixels); a
/// missing placement defaults the face to the bottom-right corner sized to the
/// canvas. An optional `tz_minutes` param sets the timezone offset (default UTC).
/// Returns `None` when no analog clock is requested (the digital label still
/// renders). Without the `overlay` feature this is never called.
#[cfg(feature = "overlay")]
fn analog_clock_from_config(
    overlays: &[mosaic_config::Overlay],
    canvas_w: u32,
    canvas_h: u32,
) -> Option<crate::overlays::AnalogClockSpec> {
    use mosaic_overlay::clock::TimeZoneOffset;

    let entry = overlays.iter().find(|o| {
        o.kind == "clock"
            && o.params
                .get("face")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|f| f.eq_ignore_ascii_case("analog"))
    })?;

    let cw = u32_to_f32(canvas_w);
    let ch = u32_to_f32(canvas_h);
    // A face sized to ~22% of the shorter canvas side by default.
    let default_radius = cw.min(ch) * 0.11;
    // Config placement is whole-pixel / whole-minute; round each param to an i32
    // and widen it losslessly to f32 (no `as` cast), or fall back to the default.
    let param_f32 = |key: &str| -> Option<f32> {
        entry
            .params
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .map(|v| i32_to_f32(round_f64_to_i32(v)))
    };
    let radius = param_f32("radius").unwrap_or(default_radius).max(8.0);
    // Default placement: bottom-right corner, inset by the radius + a margin.
    let margin = radius * 0.25;
    let cx = param_f32("x").unwrap_or(cw - radius - margin);
    let cy = param_f32("y").unwrap_or(ch - radius - margin);
    let zone = entry
        .params
        .get("tz_minutes")
        .and_then(serde_json::Value::as_f64)
        .map_or(TimeZoneOffset::UTC, |m| {
            TimeZoneOffset::from_minutes(round_f64_to_i32(m))
        });

    Some(crate::overlays::AnalogClockSpec::new(zone, cx, cy, radius))
}

/// Exact small-`u32` → `f32` widening (canvas sizes are well under `2^24`), no
/// `as` cast — mirrors [`norm_to_px_f32`]'s widener.
#[cfg(feature = "overlay")]
fn u32_to_f32(value: u32) -> f32 {
    let high = u16::try_from(value >> 16).unwrap_or(u16::MAX);
    let low = u16::try_from(value & 0xFFFF).unwrap_or(u16::MAX);
    f32::from(high) * 65_536.0 + f32::from(low)
}

/// Exact small-`i32` → `f32` widening, no `as` cast.
#[cfg(feature = "overlay")]
fn i32_to_f32(value: i32) -> f32 {
    if value < 0 {
        -u32_to_f32(value.unsigned_abs())
    } else {
        u32_to_f32(u32::try_from(value).unwrap_or(u32::MAX))
    }
}

/// Round a finite `f64` config measure to the nearest `i32` (saturating to the
/// `i32` range), no `as` cast: handle the sign, then a bounded binary search over
/// the unsigned magnitude (mirrors [`frac_to_px`]'s `u64`-bounded search).
#[cfg(feature = "overlay")]
fn round_f64_to_i32(v: f64) -> i32 {
    if !v.is_finite() {
        return 0;
    }
    let r = v.round();
    let negative = r < 0.0;
    let magnitude = r.abs().min(f64::from(i32::MAX));
    // Largest u32 candidate whose widening is <= the magnitude (so the rounded
    // value maps back exactly for integral inputs within range).
    let mut lo = 0_u32;
    let mut hi = u32::try_from(i32::MAX).unwrap_or(u32::MAX);
    while lo < hi {
        let mid = lo.saturating_add((hi - lo).saturating_add(1) / 2);
        if f64::from(mid) <= magnitude {
            lo = mid;
        } else {
            hi = mid.saturating_sub(1);
        }
    }
    let value = i32::try_from(lo).unwrap_or(i32::MAX);
    if negative {
        value.saturating_neg()
    } else {
        value
    }
}

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

/// Resolve a config [`Source`] into a streaming [`IngestPlan`] (it does **not**
/// decode anything — the plan is consumed later by an ingest thread).
///
/// `test` sources are generated up-front with the `ffmpeg` CLI (LGPL `testsrc`
/// → `mpeg2video`) into a tempdir owned by the plan; file/rtsp/hls/ts/srt/rtmp
/// sources record their path/URL to be opened on the ingest thread.
/// Live transports (rtsp/hls/ts/srt/rtmp) are flagged `live` so the ingest loop
/// reconnects on EOF/error rather than ending; `test`/`file` are finite.
///
/// # Errors
///
/// Returns [`PipelineError::Ingest`] for an NDI/unsupported source kind, or if a
/// `test` source's clip cannot be generated. (Opening/decoding errors surface on
/// the ingest thread later — they must never fail the *build* of a never-ending
/// live source.)
fn ingest_plan_for(
    source: &Source,
    tile_w: u32,
    tile_h: u32,
    store: Arc<TileStore<Nv12Image>>,
) -> Result<IngestPlan, PipelineError> {
    let mut owned = None;
    let (location, live) = match &source.kind {
        SourceKind::Test => {
            let clip = generate_test_clip(&source.id).map_err(|reason| PipelineError::Ingest {
                id: source.id.clone(),
                reason,
            })?;
            let location = SourceLocation::Path(clip.0.clone());
            owned = Some(clip);
            (location, false)
        }
        SourceKind::File { path } => (SourceLocation::Path(PathBuf::from(path)), false),
        SourceKind::Rtsp { url, .. }
        | SourceKind::Hls { url }
        | SourceKind::Ts { url }
        | SourceKind::Srt { url }
        | SourceKind::Rtmp { url } => (SourceLocation::Url(url.clone()), true),
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

    Ok(IngestPlan {
        id: source.id.clone(),
        location,
        tile_w,
        tile_h,
        store,
        live,
        #[cfg(feature = "overlay")]
        dvbsub: None,
        _owned: owned,
    })
}

/// Wire a source's native caption paths at build time: the HLS `WebVTT` rendition
/// (a second isolated demux thread) and/or the in-container DVB-sub route (#36
/// Phase 2, decoded on this source's own video-ingest thread). Registers any cue
/// store (for the baker to sample) + reader plan, and stashes the dvbsub route on
/// `plan`. Best-effort: a source whose captions cannot be resolved simply shows
/// none — this never fails the build (invariants #1/#10).
#[cfg(feature = "overlay")]
fn wire_source_captions(
    source: &Source,
    plan: &mut IngestPlan,
    caption_stores: &mut std::collections::HashMap<String, Arc<crate::captions::CueStore>>,
    caption_plans: &mut Vec<crate::captions::CaptionPlan>,
) {
    // HLS WebVTT rendition path (a second isolated demux thread).
    if let Some(caption_plan) = crate::captions::caption_plan_for(source) {
        caption_stores.insert(source.id.clone(), Arc::clone(&caption_plan.store));
        caption_plans.push(caption_plan);
    }

    // In-container DVB-sub (bitmap) path: the muxed subtitle stream is decoded on
    // THIS source's video-ingest thread (a sibling of the video packets), so the
    // route is stashed on the plan and its store registered for the baker. Only
    // when the selector takes the dvbsub path, the source has not already taken
    // the WebVTT path, and the container actually carries a dvbsub stream.
    if let Some(selector) = source.captions.as_ref() {
        if crate::captions::dvbsub_selector(&source.kind, selector)
            && !caption_stores.contains_key(&source.id)
        {
            if let Some((route, cue_store)) = resolve_dvbsub_route(source, &plan.location) {
                caption_stores.insert(source.id.clone(), cue_store);
                plan.dvbsub = Some(route);
            }
        }
    }
}

/// Resolve the in-container **DVB-sub route** for a source whose selector takes
/// the native bitmap-caption path ([`crate::captions::dvbsub_selector`]): open
/// the source container once, find its subtitle stream and confirm it is a
/// `dvbsub` stream, and build the cue store. Returns `(route, store)` so the
/// caller can both stash the route on the ingest plan AND register the store for
/// the baker to sample. Best-effort: an open failure or a container with no
/// dvbsub stream logs and returns `None` (the tile simply shows no caption — it
/// must never fail the pipeline build, invariants #1/#10).
#[cfg(feature = "overlay")]
fn resolve_dvbsub_route(
    source: &Source,
    location: &SourceLocation,
) -> Option<(DvbSubRoute, Arc<crate::captions::CueStore>)> {
    use mosaic_ffmpeg::convert::MediaKind;
    use mosaic_ffmpeg::Demuxer;

    // Only a local-path container can be opened by `Demuxer` here; a `Ts` URL
    // source is decoded by the URL ingest path and is out of this MVP's scope.
    let path = match location {
        SourceLocation::Path(p) => p.as_path(),
        SourceLocation::Url(_) => return None,
    };
    let demux = match Demuxer::open(path) {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!(source = %source.id, error = %err, "could not open container for dvbsub captions");
            return None;
        }
    };
    let stream_index = demux.best_stream(MediaKind::Subtitle)?;
    // Confirm it is a DVB-sub (bitmap) stream; teletext/other subtitle codecs are
    // not this path.
    let params = demux.streams();
    let is_dvbsub = params
        .iter()
        .find(|s| s.index == stream_index)
        .is_some_and(|s| s.codec_name == "dvbsub");
    if !is_dvbsub {
        tracing::info!(source = %source.id, "subtitle stream is not dvbsub; no in-container bitmap captions");
        return None;
    }
    let time_base = params
        .iter()
        .find(|s| s.index == stream_index)
        .map(|s| s.time_base)?;
    let store = Arc::new(crate::captions::CueStore::new());
    tracing::info!(source = %source.id, stream_index, "native in-container DVB-sub caption route resolved");
    Some((
        DvbSubRoute {
            stream_index,
            time_base,
            store: Arc::clone(&store),
        },
        store,
    ))
}

/// Where a source's media lives.
enum SourceLocation {
    /// A local filesystem path.
    Path(PathBuf),
    /// A libav-openable URL (rtsp/hls/ts/srt/rtmp).
    Url(String),
}

/// A generated test clip plus the tempdir that owns it (kept alive for as long
/// as the owning [`IngestPlan`] lives, so the ingest thread can open it).
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

/// The total budget the startup **prime-wait** ([`wait_for_prime`]) spends
/// waiting for every bound source's [`TileStore`] to publish its first frame
/// before the output clock's very first tick. Sized to cover normal decode +
/// scale latency (open container, decode the first GOP, scale to the tile) on
/// commodity hardware. CRITICAL (invariant #1/#2): this is a **hard upper
/// bound** — a source that never produces (a dead/missing/wedged input) can NOT
/// extend it; once it elapses the clock starts anyway and the unprimed tiles
/// ride their `NO_SIGNAL` / last-good placeholder ([`TileStore::read_at`] already
/// handles that). The prime-wait only ever *delays the first tick*; it never
/// changes the cadence, the per-tick logic, or makes a tile pace the output.
const PRIME_WAIT_BUDGET: Duration = Duration::from_millis(1_500);

/// How often [`wait_for_prime`] re-checks whether every store is primed. Short
/// so the clock starts promptly once the last input primes (typically well
/// before [`PRIME_WAIT_BUDGET`]); a few ms keeps the poll cheap.
const PRIME_WAIT_POLL: Duration = Duration::from_millis(5);

/// Reconnect backoff for a live source whose `open_and_stream` returned (EOF or
/// error). Capped so a flapping source retries promptly but does not hot-loop.
const INGEST_RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// How long [`IngestSupervisor::join_all`] waits for an ingest thread to observe
/// the stop flag and exit before detaching it. Generous enough that a thread in
/// a normal decode loop (which checks `stop` every packet) always joins cleanly,
/// short enough that a thread wedged in a blocking libav network call never
/// stalls the bounded run's teardown.
const INGEST_JOIN_GRACE: Duration = Duration::from_secs(2);

/// The per-source streaming-ingest loop, run on a dedicated thread (BUG-2 fix).
///
/// Opens the source, decodes its best video stream to NV12 scaled to the tile
/// size, and **publishes each frame into the store as it is decoded** — paced to
/// wall-clock by the frame's PTS (invariant #4; `-re` is never used). Returns
/// when the `stop` flag is raised (a bounded/`stop`ped run tearing ingest down)
/// or — for a finite source — when the stream ends. A `live` source reconnects
/// after [`INGEST_RECONNECT_BACKOFF`] on EOF/error, so a transient HLS/RTSP drop
/// recovers; the tile holds its last-good frame meanwhile (invariant #2). The
/// loop only ever *writes* the lock-free store, so it can neither pace nor stall
/// the output clock (invariant #1) nor back-pressure the engine (invariant #10).
fn ingest_loop(plan: &IngestPlan, stop: &AtomicBool) {
    let tag = CanvasColor::default().output_tag();
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match open_and_stream(plan, tag, stop) {
            Ok(()) => {}
            Err(reason) => {
                tracing::warn!(source = %plan.id, %reason, "ingest stream ended/errored");
            }
        }
        if !plan.live || stop.load(Ordering::Acquire) {
            // A finite source has played out (its tile now holds its last-good
            // frame forever); a stop was requested. Either way, this thread ends.
            return;
        }
        // Live source: brief backoff, then reconnect (checking `stop` in slices
        // so teardown stays prompt).
        sleep_interruptible(INGEST_RECONNECT_BACKOFF, stop);
    }
}

/// Open `plan.location`, decode its best video stream to NV12 scaled to the tile
/// size, and publish each frame into `plan.store` paced to wall-clock by PTS.
///
/// Returns `Ok(())` at clean EOF (a finite source played out), or `Err` on an
/// open/decode error. Returns early (still `Ok`) the moment `stop` is observed.
///
/// Uses `ffmpeg-next`'s safe `Input`/`Parameters` value types only to bridge the
/// container's stream parameters into `mosaic-ffmpeg`'s safe `StreamVideoDecoder`
/// (which `mosaic-ffmpeg`'s `Demuxer` does not yet surface). No `unsafe`, no FFI.
fn open_and_stream(
    plan: &IngestPlan,
    tag: mosaic_core::color::ColorInfo,
    stop: &AtomicBool,
) -> Result<(), String> {
    mosaic_ffmpeg::ensure_initialized().map_err(|e| e.to_string())?;

    let mut input = match &plan.location {
        SourceLocation::Path(p) => ffmpeg::format::input(p).map_err(|e| e.to_string())?,
        SourceLocation::Url(u) => ffmpeg::format::input(&u.as_str()).map_err(|e| e.to_string())?,
    };

    let (stream_index, params, time_base, declared_fps) = {
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or_else(|| "input has no video stream".to_owned())?;
        (
            stream.index(),
            stream.parameters(),
            mosaic_ffmpeg::from_ff_rational(stream.time_base()),
            mosaic_ffmpeg::from_ff_rational(stream.avg_frame_rate()),
        )
    };

    // Feed the declared cadence so the decoder's genpts fallback advances at the
    // source's true rate (PAL 25, film 24, …) rather than an NTSC-shaped guess;
    // an unusable rate is ignored inside `with_declared_fps` (invariant #3).
    let mut decoder = StreamVideoDecoder::new(params, time_base)
        .map_err(|e| e.to_string())?
        .with_declared_fps(Some(declared_fps));
    let mut to_tile = TileScaler::new(plan.tile_w, plan.tile_h);

    // Build the in-container DVB-sub decoder once, from the SAME open container's
    // subtitle stream parameters (#36 Phase 2). Its packets are pumped as a
    // sibling of the video packets below — they never go through `receive_frame`.
    // A build failure logs and disables the route for this open (best-effort; the
    // video still streams). Only under `overlay`.
    #[cfg(feature = "overlay")]
    let mut dvbsub = build_dvbsub_decoder(plan, &input);
    // The wall-clock pacer: maps the first frame's PTS to "now" and releases
    // each subsequent frame when wall-clock catches up to its PTS (invariant #4).
    let mut pacer = PtsWallClock::new();

    // Pump packets, publishing each decoded+scaled frame into the store.
    let mut drained = false;
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        while let Some(decoded) = decoder.receive_frame().map_err(|e| e.to_string())? {
            let image = to_tile.convert(&decoded.frame, tag)?;
            // Pace to the frame's PTS (invariant #4) so a file/VOD source is not
            // slurped into the ring faster than real time, then publish it
            // stamped with its SOURCE-RELATIVE media time — the timeline the
            // output clock latches against (latch-on-tick; see `publish_time`).
            // Re-check `stop` after the (possibly long) pace wait.
            pacer.wait_for(decoded.meta.pts, stop);
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            plan.store
                .publish(image, pacer.publish_time(decoded.meta.pts));
        }
        if drained {
            return Ok(());
        }
        let mut packet = ffmpeg::codec::packet::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {
                if packet.stream() == stream_index {
                    decoder.send_packet(&packet).map_err(|e| e.to_string())?;
                } else {
                    // A non-video packet: route it to the in-container DVB-sub
                    // decoder if it belongs to that subtitle stream (sibling
                    // branch — it never goes through the video `receive_frame`
                    // pump). A decode error on one cue is logged and skipped:
                    // captions are intermittent and must never stall ingest.
                    #[cfg(feature = "overlay")]
                    pump_dvbsub(plan, dvbsub.as_mut(), &packet);
                }
            }
            Err(ffmpeg::Error::Eof) => {
                decoder.send_eof().map_err(|e| e.to_string())?;
                drained = true;
            }
            Err(other) => return Err(other.to_string()),
        }
    }
}

/// Build the in-container DVB-sub [`CaptionDecoder`] for `plan`'s route, from the
/// open container's subtitle stream parameters. Returns `None` when the source
/// has no dvbsub route or the decoder cannot be built (logged, best-effort).
#[cfg(feature = "overlay")]
fn build_dvbsub_decoder(
    plan: &IngestPlan,
    input: &ffmpeg::format::context::Input,
) -> Option<mosaic_ffmpeg::CaptionDecoder> {
    let route = plan.dvbsub.as_ref()?;
    let params = input.stream(route.stream_index)?.parameters();
    match mosaic_ffmpeg::CaptionDecoder::from_parameters(
        mosaic_ffmpeg::CaptionSource::DvbSubtitle,
        params,
        route.time_base,
    ) {
        Ok(dec) => Some(dec),
        Err(err) => {
            tracing::warn!(source = %plan.id, error = %err, "could not build dvbsub decoder; no bitmap captions");
            None
        }
    }
}

/// Decode one packet on the in-container DVB-sub route (if the packet belongs to
/// that subtitle stream) and publish any bitmap cues into the route's store.
#[cfg(feature = "overlay")]
fn pump_dvbsub(
    plan: &IngestPlan,
    decoder: Option<&mut mosaic_ffmpeg::CaptionDecoder>,
    packet: &ffmpeg::codec::packet::Packet,
) {
    let (Some(route), Some(decoder)) = (plan.dvbsub.as_ref(), decoder) else {
        return;
    };
    if packet.stream() != route.stream_index {
        return;
    }
    match decoder.decode(packet) {
        Ok(cues) => crate::captions::publish_bitmap_cues(&route.store, cues),
        Err(err) => {
            tracing::debug!(source = %plan.id, error = %err, "dvbsub packet decode error");
        }
    }
}

/// Sleep up to `total`, waking early (in <= 50 ms slices) if `stop` is raised,
/// so ingest teardown stays prompt without a condvar.
fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let slice = Duration::from_millis(50);
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let chunk = remaining.min(slice);
        std::thread::sleep(chunk);
        remaining = remaining.saturating_sub(chunk);
    }
}

/// A wall-clock pacer keyed on frame PTS (invariant #4 — pace live/VOD by PTS,
/// never `-re`).
///
/// On the first frame it anchors `base_instant = now` to `base_pts = pts`; each
/// later frame is released when `now - base_instant >= pts - base_pts`. A frame
/// whose PTS goes backwards (a discontinuity / wrap) re-anchors rather than
/// stalling, so a misbehaving source can never block ingest for long.
struct PtsWallClock {
    anchor: Option<(Instant, MediaTime)>,
}

impl PtsWallClock {
    fn new() -> Self {
        Self { anchor: None }
    }

    /// Block (in `stop`-checked slices) until wall-clock reaches `pts`'s release
    /// instant. The first call anchors the timeline and returns immediately.
    fn wait_for(&mut self, pts: MediaTime, stop: &AtomicBool) {
        let Some((base_instant, base_pts)) = self.anchor else {
            self.anchor = Some((Instant::now(), pts));
            return;
        };
        // A backwards PTS (discontinuity / wrap) re-anchors rather than stalls.
        if pts < base_pts {
            self.anchor = Some((Instant::now(), pts));
            return;
        }
        // Target media offset from the anchor.
        let delta = pts.saturating_sub(base_pts);
        let target_ns = u64::try_from(delta.as_nanos()).unwrap_or(0);
        let target = base_instant + Duration::from_nanos(target_ns);
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            if now >= target {
                return;
            }
            let remaining = target.saturating_duration_since(now);
            std::thread::sleep(remaining.min(Duration::from_millis(50)));
        }
    }

    /// The timeline instant to stamp a published frame with: the frame's
    /// **source-relative media time** (`pts - first_pts`), i.e. how far into the
    /// clip this frame sits, measured from the first decoded frame.
    ///
    /// This is the timeline the output clock samples against (latch-on-tick,
    /// streaming-gotchas §1): output tick `N` latches the source frame whose
    /// source-relative media time is nearest-but-not-after `N * tick_period`, so
    /// the tile advances exactly 1:1 with output media time regardless of how
    /// fast the producer decoded. Stamping wall-clock-elapsed here instead would
    /// re-introduce the race whenever the output loop runs slower than real time
    /// (the producer would have published far ahead of the output's own clock).
    ///
    /// A frame whose PTS precedes the anchor (a re-anchor case the caller already
    /// handles in [`Self::wait_for`]) clamps to zero rather than going negative.
    fn publish_time(&self, pts: MediaTime) -> MediaTime {
        match self.anchor {
            Some((_, base_pts)) => pts.saturating_sub(base_pts),
            None => MediaTime::ZERO,
        }
    }
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

#[cfg(all(test, feature = "overlay"))]
mod overlay_clock_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// Build a `clock` overlay carrying `params` by deserializing JSON (the
    /// `#[non_exhaustive]` `Overlay` has no cross-crate struct literal).
    fn clock_overlay(params: serde_json::Value) -> mosaic_config::Overlay {
        let mut obj = serde_json::Map::new();
        obj.insert("id".to_owned(), serde_json::json!("clk"));
        obj.insert("kind".to_owned(), serde_json::json!("clock"));
        obj.insert("target".to_owned(), serde_json::json!("canvas"));
        if let serde_json::Value::Object(extra) = params {
            for (k, v) in extra {
                obj.insert(k, v);
            }
        }
        serde_json::from_value(serde_json::Value::Object(obj)).expect("clock overlay deserializes")
    }

    #[test]
    fn no_clock_overlay_yields_none() {
        assert!(analog_clock_from_config(&[], 1280, 720).is_none());
        // A digital clock overlay does NOT request the analog face.
        let digital = clock_overlay(serde_json::json!({ "face": "digital" }));
        assert!(analog_clock_from_config(&[digital], 1280, 720).is_none());
    }

    #[test]
    fn analog_face_param_requests_the_face() {
        let analog = clock_overlay(serde_json::json!({ "face": "analog" }));
        let spec = analog_clock_from_config(&[analog], 1280, 720)
            .expect("an analog clock overlay yields a spec");
        // Default placement is the bottom-right quadrant of the canvas.
        assert!(
            spec.cx() > 640.0 && spec.cy() > 360.0,
            "default to bottom-right: {spec:?}"
        );
        assert!(spec.radius() >= 8.0, "radius is sane");
    }

    #[test]
    fn explicit_placement_params_are_honoured() {
        let analog = clock_overlay(
            serde_json::json!({ "face": "analog", "x": 200, "y": 150, "radius": 64 }),
        );
        let spec = analog_clock_from_config(&[analog], 1280, 720).unwrap();
        assert!((spec.cx() - 200.0).abs() < 0.5, "explicit x honoured");
        assert!((spec.cy() - 150.0).abs() < 0.5, "explicit y honoured");
        assert!(
            (spec.radius() - 64.0).abs() < 0.5,
            "explicit radius honoured"
        );
    }
}

#[cfg(all(test, feature = "overlay"))]
mod fault_detector_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::overlays::TileFault;

    /// A 25/1 fps cadence (40 ms per tick), so the dwell windows below convert to
    /// a small, deterministic number of ticks.
    fn cadence() -> Rational {
        Rational { num: 25, den: 1 }
    }

    /// The output media instant of tick `i` at 25 fps (exact, integer ns).
    fn pts_of(i: u64) -> MediaTime {
        MediaTime::from_nanos(
            i64::try_from(i)
                .unwrap_or(i64::MAX)
                .saturating_mul(40_000_000),
        )
    }

    /// A solid 64x64 NV12 image at luma `y` (chroma neutral), canvas-tagged.
    fn solid(y: u8) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        Nv12Image::solid(64, 64, y, 128, 128, tag).expect("solid frame")
    }

    /// A 64x64 NV12 image whose luma carries a per-pixel gradient seeded by
    /// `seed`, so two frames with different seeds differ in (nearly) every sample
    /// — a genuinely *moving*, *bright* picture (never black, never frozen).
    fn moving(seed: u8) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        let mut y = vec![0_u8; 64 * 64];
        for (i, px) in y.iter_mut().enumerate() {
            // Bright base (well above the black threshold) + a seed-varying ramp.
            let ramp = u8::try_from(i % 200).unwrap_or(0);
            *px = 120u8
                .saturating_add(ramp / 4)
                .wrapping_add(seed.wrapping_mul(37));
        }
        let uv = vec![128_u8; 64 * 64 / 2];
        Nv12Image::new(64, 64, y, uv, tag).expect("moving frame")
    }

    /// A 64x64 NV12 frame for a **near-frozen, real-codec-like** source: a fixed
    /// bright base picture (Y=180) perturbed by two kinds of change keyed on the
    /// frame `tick`, simulating what the `FreezeProbe` actually sees off an encoded
    /// frozen feed (see the instrumentation in the commit message):
    ///
    ///  * every frame, a tiny ±2-level dither on a sparse set of samples — well
    ///    inside the probe's per-sample tolerance, so it reads as unchanged;
    ///  * every 25th frame (a simulated GOP boundary), a LARGER ±12-level shift on
    ///    ~1.5 % of samples — beyond the per-sample tolerance, so that single frame
    ///    spikes the changed-fraction above the freeze threshold.
    ///
    /// A correct detector must still raise FROZEN: the lone per-GOP spike must not
    /// reset the dwell. The pre-fix detector (engine-default probe, no debounce)
    /// would NOT — this is the regression this frame shape guards.
    fn near_frozen(tick: u64) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        let mut y = vec![180_u8; 64 * 64];
        // Per-frame tiny dither (±2, within tolerance) on every 7th sample, phase
        // by tick so it genuinely differs frame-to-frame but stays "unchanged".
        let phase = u8::try_from(tick % 2).unwrap_or(0);
        for (i, px) in y.iter_mut().enumerate() {
            if i % 7 == 0 {
                *px = if phase == 0 { 180 } else { 182 };
            }
        }
        // Simulated GOP boundary every 25 frames: a larger ±12 shift on a band of
        // samples (~1.6 % of the 4096) — a single-frame spike above tolerance.
        if tick % 25 == 0 && tick > 0 {
            for px in y.iter_mut().take(64) {
                *px = 168; // 180 - 12, beyond the ±6 tolerance
            }
        }
        let uv = vec![128_u8; 64 * 64 / 2];
        Nv12Image::new(64, 64, y, uv, tag).expect("near-frozen frame")
    }

    /// A 64x64 NV12 frame for a **barely-moving** source: a fixed bright base with
    /// a small moving band that changes by more than the per-sample tolerance on
    /// ~0.5 % of samples EVERY frame (above the 0.1 % freeze threshold), keyed on
    /// `tick`. This mimics the real silent demo source (a moving testsrc scaled to
    /// a tile: continuous small motion, instrumented median ~0.23 % changed). It
    /// must NOT be flagged frozen — it is the over-loosening guard for the freeze
    /// threshold (a 0.5 % threshold would wrongly call this frozen).
    fn barely_moving(tick: u64) -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        let mut y = vec![160_u8; 64 * 64];
        // ~0.5 % of 4096 = ~20 samples flip between two well-separated values each
        // frame (diff 40 >> tolerance), giving a steady changed-fraction ~0.5 %.
        let bright = u8::try_from(tick % 2).unwrap_or(0) == 0;
        for px in y.iter_mut().take(20) {
            *px = if bright { 200 } else { 60 };
        }
        let uv = vec![128_u8; 64 * 64 / 2];
        Nv12Image::new(64, 64, y, uv, tag).expect("barely-moving frame")
    }

    /// Build a single-source store map keyed by `id`, holding-forever so a frozen
    /// source keeps reporting its last-good frame (matches the pipeline).
    fn store_for(id: &str) -> (HashMapStores, Arc<TileStore<Nv12Image>>) {
        let store = Arc::new(TileStore::new(
            id.to_owned(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ));
        let mut stores: HashMapStores = std::collections::HashMap::new();
        stores.insert(id.to_owned(), Arc::clone(&store));
        (stores, store)
    }

    /// The states map naming `id` as a LIVE cell-bound source (what `sample`
    /// iterates over).
    fn live_states(
        id: &str,
    ) -> std::collections::HashMap<String, mosaic_core::traits::SourceState> {
        let mut s = std::collections::HashMap::new();
        s.insert(id.to_owned(), mosaic_core::traits::SourceState::Live);
        s
    }

    #[test]
    fn sustained_all_black_frames_raise_a_black_fault() {
        let id = "blk";
        let (stores, store) = store_for(id);
        let mut det = FaultDetector::new(stores, std::collections::HashMap::new(), cadence());
        let states = live_states(id);

        // Drive 40 ticks (1.6 s > the 0.5 s black dwell) publishing an all-black
        // (Y=16) frame each tick so the tile stays LIVE and black.
        let mut last = std::collections::HashMap::new();
        for i in 0..40 {
            store.publish(solid(16), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Black),
            "an all-black source sustained past the dwell must raise a BLACK fault"
        );
    }

    #[test]
    fn sustained_identical_frames_raise_a_frozen_fault() {
        let id = "frz";
        let (stores, store) = store_for(id);
        let mut det = FaultDetector::new(stores, std::collections::HashMap::new(), cadence());
        let states = live_states(id);

        // Publish the SAME bright, non-black content every tick (Y=200 solid):
        // successive frames are identical → frozen. Drive 70 ticks (2.8 s > the
        // 2 s freeze dwell).
        let mut last = std::collections::HashMap::new();
        for i in 0..70 {
            store.publish(solid(200), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Frozen),
            "an unchanging bright source past the freeze dwell must raise a FROZEN fault"
        );
    }

    #[test]
    fn sustained_quiet_meter_raises_a_silent_fault() {
        let id = "sil";
        let (stores, store) = store_for(id);
        // A meter timeline pinned below the silence floor for every tick.
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-80.0_f64; 80]);
        let mut det = FaultDetector::new(stores, timelines, cadence());
        let states = live_states(id);

        // Publish a MOVING, bright picture so neither black nor freeze fires —
        // only silence should. Drive 30 ticks (1.2 s > the 0.5 s silence dwell).
        let mut last = std::collections::HashMap::new();
        for i in 0..30 {
            store.publish(moving(u8::try_from(i % 251).unwrap_or(0)), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Silent),
            "a moving bright source with a sustained-quiet meter must raise a SILENT fault"
        );
    }

    #[test]
    fn moving_bright_loud_source_reports_no_fault() {
        let id = "ok";
        let (stores, store) = store_for(id);
        // A loud meter timeline (well above the silence floor) for every tick.
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-6.0_f64; 80]);
        let mut det = FaultDetector::new(stores, timelines, cadence());
        let states = live_states(id);

        // Publish a MOVING, bright picture (changes every tick) and a loud meter:
        // no content fault should ever raise across a long run.
        for i in 0..70 {
            store.publish(moving(u8::try_from(i % 251).unwrap_or(0)), pts_of(i));
            let faults = det.sample(pts_of(i), i, &states);
            assert!(
                faults.get(id).copied().unwrap_or(TileFault::None) == TileFault::None,
                "a healthy moving+bright+loud source must never raise a fault (tick {i})"
            );
        }
    }

    #[test]
    fn near_frozen_source_with_gop_noise_still_raises_frozen() {
        // REGRESSION (commit 08bb78a defect): a genuinely frozen feed delivered as
        // ENCODED video is not byte-identical — each GOP boundary perturbs a small
        // fraction of luma. The first cut shipped the engine-default freeze probe
        // (0.1 % threshold, tol 2) with no debounce, so each per-GOP spike reset
        // the 2 s dwell and the FROZEN badge never appeared on the real source.
        // This drives the SAME near-frozen + per-GOP-spike shape and asserts the
        // tuned probe + debounce still raise FROZEN after the dwell.
        let id = "nearfrz";
        let (stores, store) = store_for(id);
        // Loud meter so silence never fires — freeze must be the only fault.
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-6.0_f64; 120]);
        let mut det = FaultDetector::new(stores, timelines, cadence());
        let states = live_states(id);

        // Drive 90 ticks (3.6 s > the 2 s freeze dwell), crossing >=3 simulated
        // GOP boundaries (ticks 25/50/75) so the dwell must survive the spikes.
        let mut last = std::collections::HashMap::new();
        for i in 0..90 {
            store.publish(near_frozen(i), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Frozen),
            "a near-frozen encoded source (per-GOP noise spikes) must still raise FROZEN"
        );
    }

    #[test]
    fn barely_moving_quiet_source_is_silent_not_frozen() {
        // OVER-LOOSENING guard (the second-order defect found while fixing the
        // first): a source with small-but-continuous motion and a quiet meter is
        // SILENT, never FROZEN. An over-loose freeze threshold (e.g. 0.5 %) would
        // wrongly call this real moving-but-silent feed frozen.
        let id = "barely";
        let (stores, store) = store_for(id);
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-80.0_f64; 120]);
        let mut det = FaultDetector::new(stores, timelines, cadence());
        let states = live_states(id);

        let mut last = std::collections::HashMap::new();
        for i in 0..90 {
            store.publish(barely_moving(i), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Silent),
            "a barely-moving but quiet source must read SILENT, not FROZEN"
        );
    }

    #[test]
    fn black_takes_precedence_over_silence() {
        // A source that is BOTH black AND silent surfaces BLACK (the higher-
        // precedence content fault), not SILENT.
        let id = "both";
        let (stores, store) = store_for(id);
        let mut timelines = std::collections::HashMap::new();
        timelines.insert(id.to_owned(), vec![-80.0_f64; 80]);
        let mut det = FaultDetector::new(stores, timelines, cadence());
        let states = live_states(id);

        let mut last = std::collections::HashMap::new();
        for i in 0..40 {
            store.publish(solid(16), pts_of(i));
            last = det.sample(pts_of(i), i, &states);
        }
        assert_eq!(
            last.get(id).copied(),
            Some(TileFault::Black),
            "black must outrank silence when both conditions hold"
        );
    }
}

/// Deterministic tests for the startup **prime-wait** ([`wait_for_prime`]): the
/// #40 fix that holds the very first output tick until every bound tile has
/// published its first frame — but only for a bounded budget, so a dead source
/// can never block startup (invariant #1/#2). The wait's clock + sleep are
/// injected, so these exercise the budget/poll behaviour with NO real sleeping.
#[cfg(test)]
mod prime_wait_tests {
    use std::cell::Cell as StdCell;
    use std::sync::Arc;
    use std::time::Duration;

    use mosaic_compositor::pipeline::{CanvasColor, Nv12Image};
    use mosaic_core::time::MediaTime;
    use mosaic_engine::ManualTimeSource;
    use mosaic_framestore::{NoSignalPolicy, TileStore, TileThresholds};

    use super::{wait_for_prime, PRIME_WAIT_POLL};

    /// A tiny NV12 frame to publish so a store reads primed.
    fn frame() -> Nv12Image {
        let tag = CanvasColor::default().output_tag();
        Nv12Image::solid(2, 2, 16, 128, 128, tag).expect("solid nv12")
    }

    /// A fresh (cold, unprimed) tile store.
    fn store(id: &str) -> Arc<TileStore<Nv12Image>> {
        Arc::new(TileStore::new(
            id.to_owned(),
            TileThresholds::default(),
            NoSignalPolicy::HoldForever,
        ))
    }

    #[test]
    fn proceeds_immediately_when_all_stores_are_primed() {
        // Every store has already published a frame: the wait must return at once
        // (waited == 0), report all_primed, and never sleep.
        let a = store("a");
        let b = store("b");
        a.publish(frame(), MediaTime::from_nanos(0));
        b.publish(frame(), MediaTime::from_nanos(0));
        let stores = [&a, &b];

        let clock = ManualTimeSource::new();
        let slept = StdCell::new(false);
        let budget = Duration::from_millis(1_500);
        let outcome = wait_for_prime(&stores, budget, PRIME_WAIT_POLL, &clock, |_| {
            slept.set(true);
        });

        assert!(outcome.all_primed, "all-primed must report all_primed");
        assert_eq!(outcome.primed, 2);
        assert_eq!(outcome.total, 2);
        assert_eq!(
            outcome.waited,
            Duration::ZERO,
            "an already-primed set must not delay the first tick"
        );
        assert!(
            !slept.get(),
            "must not sleep when everything is already primed"
        );
    }

    #[test]
    fn proceeds_after_timeout_when_a_store_never_primes() {
        // One store primes; the other NEVER does (the deliberately-missing /
        // dead-source case). The wait MUST NOT hang: once the budget elapses it
        // returns all_primed == false so the caller starts the clock anyway. The
        // injected sleep advances the manual clock by exactly the requested nap,
        // so the budget is reached deterministically with zero real sleeping.
        let a = store("a");
        let dead = store("dead");
        a.publish(frame(), MediaTime::from_nanos(0));
        // `dead` is intentionally never published into.
        let stores = [&a, &dead];

        let clock = ManualTimeSource::new();
        let naps = StdCell::new(0_u32);
        let budget = Duration::from_millis(1_500);
        let outcome = wait_for_prime(&stores, budget, PRIME_WAIT_POLL, &clock, |nap| {
            // Drive the SAME clock the wait measures against, so the budget is
            // actually reached — this is what proves the loop terminates.
            clock.advance(nap);
            naps.set(naps.get().saturating_add(1));
        });

        assert!(
            !outcome.all_primed,
            "a never-priming source must NOT keep all_primed true (no infinite wait)"
        );
        assert_eq!(outcome.primed, 1, "only the live source primed");
        assert_eq!(outcome.total, 2);
        assert!(
            outcome.waited >= budget,
            "the wait must spend (at least) the full budget before giving up, got {:?}",
            outcome.waited
        );
        // It must have actually polled/slept a bounded number of times to get
        // there (budget / poll), proving it neither span hot nor hung.
        assert!(naps.get() > 0, "must have polled at least once");
        let max_naps = (budget.as_nanos() / PRIME_WAIT_POLL.as_nanos()).max(1) + 2;
        assert!(
            u128::from(naps.get()) <= max_naps,
            "poll count {} must be bounded by ~budget/poll ({max_naps})",
            naps.get()
        );
    }

    #[test]
    fn no_bound_stores_does_not_delay_the_clock() {
        // A run with no cell-bound source (degenerate) must not wait at all.
        let stores: [&Arc<TileStore<Nv12Image>>; 0] = [];
        let clock = ManualTimeSource::new();
        let outcome = wait_for_prime(
            &stores,
            Duration::from_millis(1_500),
            PRIME_WAIT_POLL,
            &clock,
            |_| panic!("must never sleep with no stores"),
        );
        assert!(outcome.all_primed);
        assert_eq!(outcome.total, 0);
        assert_eq!(outcome.waited, Duration::ZERO);
    }
}
