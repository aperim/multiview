//! Real encode-once-mux-many output sinks (the `ffmpeg` feature).
//!
//! This module turns a stream of composited frames into encoded output, two
//! ways, sharing **one** encoder per rendition (invariant #7,
//! encode-once-mux-many):
//!
//! - [`FileSink`] — encode the canvas once and mux every packet into a single
//!   container file.
//! - [`SegmentSink`] — encode the canvas once and split the *same* packet
//!   stream into GOP-aligned segments (one self-contained MPEG-TS file each),
//!   feeding each completed segment into the pure-Rust
//!   [`MediaPlaylist`](crate::hls::MediaPlaylist) so the playlist references
//!   exactly the segments written.
//!
//! The encoder and muxer are built and driven entirely through
//! [`mosaic_ffmpeg`]'s **safe** wrappers
//! ([`VideoEncoder`](mosaic_ffmpeg::VideoEncoder) /
//! [`Muxer`](mosaic_ffmpeg::Muxer)); this crate never touches libav directly and
//! stays `unsafe_code = forbid`. It never even *names* a raw libav packet type —
//! encoded packets flow from `receive_packet` straight into `write_packet` with
//! their type inferred.
//!
//! ## Timestamps (invariants #1/#3)
//!
//! Frames are presented to the encoder with a PTS computed **from the output
//! tick counter** — `out_pts = tick` in the encoder time-base (the reciprocal
//! of the output cadence). Raw input PTS is never forwarded to the encoder or
//! muxer; the source's per-frame timestamps are deliberately overwritten here.
//!
//! ## Licensing (LGPL-clean default)
//!
//! [`EncodeConfig::codec_name`] must name an LGPL software codec
//! (`"mpeg2video"`, `"ffv1"`, `"mjpeg"`, `"rawvideo"`). The GPL `x264`/`x265`
//! encoders are reserved for the separate `gpl-codecs` feature and are never
//! reachable through `ffmpeg` alone. Tests and the default path use
//! `mpeg2video`.

use std::path::{Path, PathBuf};

use ffmpeg_next::format::Pixel;
use ffmpeg_next::util::frame::Video;
use mosaic_core::time::{rescale, MediaTime, Rational};
use mosaic_ffmpeg::{DecodedVideoFrame, Muxer, ScaleSpec, Scaler, VideoEncodeTarget, VideoEncoder};

use crate::error::{Error, Result};
use crate::hls::{MediaPlaylist, Segment, SegmentType};

/// Nanoseconds in one second (the internal timeline unit, invariant #3).
const NANOS_PER_SEC: i64 = 1_000_000_000;

/// Test-only seam: counts how many *seed* encoders the segment sink builds
/// across a run. The seed encoder exists solely to copy codec parameters onto a
/// freshly-opened segment muxer; under encode-once-mux-many (invariant #7) the
/// codec is fixed for the whole run, so this must be built **once** regardless
/// of how many segments are produced. The unit tests assert exactly one build
/// per run; production code only ever increments it.
#[cfg(test)]
static SEED_ENCODER_BUILDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that one seed encoder was built (test-only instrumentation).
#[cfg(test)]
fn note_seed_encoder_built() {
    SEED_ENCODER_BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// No-op in non-test builds: the seed-encoder counter is test-only.
#[cfg(not(test))]
const fn note_seed_encoder_built() {}

/// Test-only seam: counts how many segment muxers the segment sink *finalizes*
/// (writes the trailer for) across a run. The finalize-on-error fix must finish
/// the currently-open segment before propagating a mid-run error, so on the
/// error path this must equal the number of segments that were opened — not one
/// fewer (the bug left the last open segment un-finalized). MPEG-TS has no
/// load-bearing trailer, so this counter is the faithful structural signal the
/// fix is observable through.
#[cfg(test)]
static SEGMENT_FINALIZES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that one segment muxer was finalized (test-only instrumentation).
#[cfg(test)]
fn note_segment_finalized() {
    SEGMENT_FINALIZES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// No-op in non-test builds: the segment-finalize counter is test-only.
#[cfg(not(test))]
const fn note_segment_finalized() {}

/// A source of composited frames to encode.
///
/// The engine implements this over the compositor's program output; tests
/// implement it over decoded test frames. Each call yields the next frame to
/// present, or `Ok(None)` when the program is exhausted (a finite test run; a
/// live engine never ends).
///
/// The yielded frame's own timestamps are ignored: the sink re-stamps each
/// frame's PTS from the output tick counter (invariant #3) before encoding.
pub trait VideoFrameSource {
    /// Pull the next composited frame, or `Ok(None)` at end of program.
    ///
    /// # Errors
    /// Returns an [`Error`] if the underlying source failed to produce a frame.
    fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>>;
}

/// Map a `mosaic-ffmpeg` error onto this crate's output error taxonomy.
// reason: takes the error by value so it can be used directly as
// `.map_err(ff)` (which hands ownership of the error); the body only needs a
// reference, hence the lint, but a `&` signature would force a closure at every
// call site.
#[allow(clippy::needless_pass_by_value)]
fn ff(err: mosaic_ffmpeg::FfmpegError) -> Error {
    Error::Output(err.to_string())
}

/// Configuration for the single per-rendition encoder shared by both sinks.
///
/// `cadence` is the output clock's frames-per-second as an exact rational
/// (e.g. [`Rational::FPS_30`] or `60000/1001`) — never a float fps (invariant
/// #3). The encoder time-base is its reciprocal.
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    /// LGPL software codec short name (`"mpeg2video"`, `"ffv1"`, `"mjpeg"`,
    /// `"rawvideo"`). GPL encoders are out of scope for the default build.
    pub codec_name: String,
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// The pixel format fed to the encoder. The pipeline carries frames as NV12
    /// (invariant #5); this is the format the chosen codec accepts, and frames
    /// are converted NV12 -> this immediately before encoding. LGPL software
    /// codecs (`mpeg2video`, `mjpeg`, `ffv1`) want planar `yuv420p`.
    pub format: Pixel,
    /// Output cadence (frames per second) as an exact rational.
    pub cadence: Rational,
    /// Keyframe interval in frames (GOP size). For GOP-aligned segmenting this
    /// is also the per-segment frame count. Must be non-zero.
    pub gop: u32,
    /// Target bitrate in bits/sec (`0` lets the codec choose).
    pub bit_rate: usize,
}

impl EncodeConfig {
    /// A sensible LGPL-clean default for tests/examples: `mpeg2video` fed
    /// `yuv420p`, 30 fps, the given geometry, a one-second GOP.
    #[must_use]
    pub fn mpeg2(width: u32, height: u32) -> Self {
        Self {
            codec_name: "mpeg2video".to_owned(),
            width,
            height,
            format: Pixel::YUV420P,
            cadence: Rational::FPS_30,
            gop: 30,
            bit_rate: 2_000_000,
        }
    }

    /// The encoder time-base: the reciprocal of the cadence (seconds per tick),
    /// so a frame at tick `n` carries PTS `n`.
    #[must_use]
    pub fn time_base(&self) -> Rational {
        Rational::new(self.cadence.den, self.cadence.num)
    }

    /// Build the `mosaic-ffmpeg` encode target for this configuration.
    fn target(&self) -> VideoEncodeTarget {
        VideoEncodeTarget {
            codec_name: self.codec_name.clone(),
            width: self.width,
            height: self.height,
            format: self.format,
            time_base: self.time_base(),
            bit_rate: self.bit_rate,
            gop: self.gop,
        }
    }

    /// Validate the configuration before opening an encoder.
    fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::Output(
                "encode canvas has a zero dimension".to_owned(),
            ));
        }
        if self.gop == 0 {
            return Err(Error::Output("encode GOP size must be non-zero".to_owned()));
        }
        if self.cadence.num <= 0 || self.cadence.den <= 0 {
            return Err(Error::Output("output cadence must be positive".to_owned()));
        }
        Ok(())
    }
}

/// Lazily-built NV12 -> encoder-format converter, reused across frames.
///
/// The compositor's program output is NV12 (invariant #5), but LGPL software
/// encoders want planar `yuv420p`; this performs that one conversion immediately
/// before `send_frame`, via `mosaic-ffmpeg`'s safe [`Scaler`] (libswscale). The
/// scaler is built on the first frame and rebuilt only if the geometry/format
/// changes. When the source frame already matches the encoder format, the frame
/// is passed through with only its PTS re-stamped.
struct FrameConverter {
    dst: Pixel,
    scaler: Option<Scaler>,
}

impl FrameConverter {
    fn new(dst: Pixel) -> Self {
        Self { dst, scaler: None }
    }

    /// Convert `frame` to the encoder format if needed and stamp it with `pts`
    /// (from the output tick counter — invariants #1/#3).
    fn prepare(&mut self, frame: Video, pts: i64) -> Result<Video> {
        let mut out = if frame.format() == self.dst {
            frame
        } else {
            let src = ScaleSpec::new(frame.format(), frame.width(), frame.height());
            let dst = ScaleSpec::new(self.dst, frame.width(), frame.height());
            let rebuild = match &self.scaler {
                Some(s) => s.source() != src || s.destination() != dst,
                None => true,
            };
            if rebuild {
                self.scaler = Some(Scaler::new(src, dst).map_err(ff)?);
            }
            let scaler = self
                .scaler
                .as_mut()
                .ok_or_else(|| Error::Output("frame converter unexpectedly absent".to_owned()))?;
            scaler.run(&frame).map_err(ff)?
        };
        // Re-stamp from the tick counter; the source's input PTS is discarded.
        out.set_pts(Some(pts));
        Ok(out)
    }
}

/// Counters describing one encode run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EncodeStats {
    /// Total encoded packets produced.
    pub packets: u64,
    /// How many of those packets were keyframes.
    pub keyframes: u64,
}

/// A sink that encodes the canvas once and muxes every packet into a single
/// container file (`.mkv`, `.mp4`, `.ts`, …, inferred from the extension).
pub struct FileSink {
    config: EncodeConfig,
    path: PathBuf,
}

impl FileSink {
    /// Create a file sink that will write the encoded program to `path`.
    #[must_use]
    pub fn new(config: EncodeConfig, path: impl Into<PathBuf>) -> Self {
        Self {
            config,
            path: path.into(),
        }
    }

    /// Encode the entire `source` to the configured container file and return
    /// the encode statistics.
    ///
    /// One encoder, one muxer stream: composite/decode once, encode once, mux
    /// every packet (invariant #7).
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the encoder/muxer fails or the source errors.
    pub fn run<S: VideoFrameSource>(&self, source: &mut S) -> Result<EncodeStats> {
        self.config.validate()?;
        let mut encoder = VideoEncoder::new(&self.config.target()).map_err(ff)?;
        let time_base = encoder.time_base();
        let mut muxer = Muxer::create(&self.path).map_err(ff)?;
        let stream_index = muxer
            .add_stream(encoder.as_codec_context(), time_base)
            .map_err(ff)?;
        muxer.write_header().map_err(ff)?;
        // Composite/decode once, encode once, mux every packet to the single
        // container stream (invariant #7).
        let driven = drive_to_single_muxer(
            &mut encoder,
            &mut muxer,
            stream_index,
            self.config.format,
            source,
        );
        // Finalize-on-error: always write the trailer (best-effort) before
        // returning, so even a mid-run source/encoder error leaves a structurally
        // valid container (e.g. an MP4 with its moov atom) rather than a file a
        // player cannot open. `Muxer::finish` is idempotent.
        finalize_or_propagate(&mut muxer, driven)
    }

    /// The path this sink writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Finalize a single-stream muxer after its drive loop, preserving error
/// priority. On success the trailer is written and a finish failure is surfaced;
/// on a drive failure the trailer is still written **best-effort** (so the
/// container is structurally valid) but the original drive error is the one
/// returned — a finish failure on an already-failing run is intentionally
/// dropped (the muxer is finalized as far as libav could manage).
///
/// `Muxer::finish` is idempotent, so calling it here is safe even though the
/// success path already finalized in earlier revisions.
fn finalize_or_propagate(muxer: &mut Muxer, driven: Result<EncodeStats>) -> Result<EncodeStats> {
    match driven {
        Ok(stats) => {
            muxer.finish().map_err(ff)?;
            Ok(stats)
        }
        Err(err) => {
            // Best-effort finalize; the drive error wins. Match (not `let _ =`)
            // to make the deliberate drop explicit rather than a silent discard.
            match muxer.finish() {
                Ok(()) | Err(_) => {}
            }
            Err(err)
        }
    }
}

/// Drive the shared encode-once-mux-many loop: pull frames from `source`,
/// convert NV12 -> the encoder format, re-stamp each PTS from the tick counter
/// (invariants #1/#3), encode once, and write every packet to `muxer` on the
/// single registered `stream_index`. Flushes the encoder at end-of-source. The
/// caller writes the muxer header before and the trailer after.
///
/// Both [`FileSink`] and [`PushSink`] share this so a container file and a
/// live push are the *same* one-encode stream fanned to different muxers
/// (invariant #7) — never a per-output re-encode.
fn drive_to_single_muxer<S: VideoFrameSource>(
    encoder: &mut VideoEncoder,
    muxer: &mut Muxer,
    stream_index: usize,
    format: Pixel,
    source: &mut S,
) -> Result<EncodeStats> {
    let mut tick: i64 = 0;
    let mut stats = EncodeStats::default();
    let mut converter = FrameConverter::new(format);
    while let Some(frame) = source.next_frame()? {
        // Convert NV12 -> encoder format and re-stamp PTS from the tick
        // counter: out_pts = f(tick) (inv #1/#3).
        let prepared = converter.prepare(frame.frame, tick)?;
        encoder.send_frame(&prepared).map_err(ff)?;
        // `packet`'s type is inferred — this crate never names a libav type.
        while let Some(packet) = encoder.receive_packet().map_err(ff)? {
            record(&mut stats, packet.is_key());
            muxer.write_packet(stream_index, packet).map_err(ff)?;
        }
        tick = tick.saturating_add(1);
    }
    encoder.send_eof().map_err(ff)?;
    while let Some(packet) = encoder.receive_packet().map_err(ff)? {
        record(&mut stats, packet.is_key());
        muxer.write_packet(stream_index, packet).map_err(ff)?;
    }
    Ok(stats)
}

/// Result of a [`SegmentSink`] run: the segments written and the playlist that
/// references them.
#[derive(Debug)]
pub struct SegmentResult {
    /// Absolute paths of the segment files written, in order.
    pub segments: Vec<PathBuf>,
    /// The media playlist referencing those segments (already populated).
    pub playlist: MediaPlaylist,
    /// Encode statistics for the run.
    pub stats: EncodeStats,
}

/// A sink that encodes the canvas once and splits the *same* packet stream into
/// GOP-aligned MPEG-TS segments, building the HLS media playlist that
/// references them (invariant #7: one encode, many segments).
///
/// Each segment is a self-contained MPEG-TS file that begins on a keyframe, so a
/// player can decode any segment independently. Segments are written into
/// `dir`; the populated playlist is returned for the caller to write alongside
/// them.
pub struct SegmentSink {
    config: EncodeConfig,
    dir: PathBuf,
    prefix: String,
}

impl SegmentSink {
    /// Create a segment sink writing `prefix{n}.ts` segments into `dir`.
    #[must_use]
    pub fn new(config: EncodeConfig, dir: impl Into<PathBuf>, prefix: impl Into<String>) -> Self {
        Self {
            config,
            dir: dir.into(),
            prefix: prefix.into(),
        }
    }

    /// Encode `source` once, segmenting the packet stream at keyframe
    /// boundaries, writing each segment as an MPEG-TS file and recording it in
    /// the returned [`MediaPlaylist`].
    ///
    /// A new segment begins whenever a keyframe packet is produced (the encoder
    /// is configured with a fixed GOP, so this is deterministic and
    /// GOP-aligned). The first packet is always a keyframe.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if the encoder/muxer fails, the source errors,
    /// or the encoder produces a non-keyframe before any keyframe (a degenerate
    /// configuration).
    pub fn run<S: VideoFrameSource>(&self, source: &mut S) -> Result<SegmentResult> {
        self.config.validate()?;
        let mut encoder = VideoEncoder::new(&self.config.target()).map_err(ff)?;
        let time_base = encoder.time_base();
        let frame_ns = rescale(1, time_base, Rational::new(1, NANOS_PER_SEC));

        let mut state = SegmentState::new(&self.config, &self.dir, &self.prefix, time_base)?;
        let mut converter = FrameConverter::new(self.config.format);

        // Drive the encode/segment loop, capturing any mid-run failure so the
        // currently-open segment can be finalized best-effort before the error
        // propagates (finalize-on-error). On the error path we do not build the
        // playlist; we only ensure the open segment's container is closed.
        if let Err(err) = state.drive(&mut encoder, &mut converter, source) {
            state.finalize_open_segment_best_effort();
            return Err(err);
        }

        let mut playlist = MediaPlaylist::new(SegmentType::MpegTs);
        state.finish(&mut playlist, frame_ns)?;
        // TARGETDURATION must be >= the longest EXTINF, as an integer.
        playlist.recompute_target_duration();
        playlist.set_finished(true);

        let run_stats = state.stats;
        Ok(SegmentResult {
            segments: state.into_segment_paths(),
            playlist,
            stats: run_stats,
        })
    }

    /// The directory segments are written into.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// A live push transport: the container/muxer libav uses to stream the encoded
/// program to a remote peer over the matching protocol.
///
/// The protocol fixes the on-the-wire container the same way a file extension
/// fixes a file's: RTMP carries FLV, MPEG-TS-over-{SRT,UDP,RTP} carries an
/// MPEG-TS, and RTSP its own framing. The selected libav muxer name is an
/// implementation detail surfaced by [`PushProtocol::muxer_name`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PushProtocol {
    /// RTMP push (`rtmp://…`) — FLV-framed, the common ingest protocol.
    Rtmp,
    /// SRT push (`srt://…`) — an MPEG-TS payload over the SRT transport.
    Srt,
    /// RTSP announce/record (`rtsp://…`).
    Rtsp,
    /// Raw MPEG-TS over UDP (`udp://…`).
    UdpTs,
}

impl PushProtocol {
    /// The libav output-muxer short-name this protocol streams through.
    #[must_use]
    pub const fn muxer_name(self) -> &'static str {
        match self {
            Self::Rtmp => "flv",
            // SRT and plain UDP both carry an MPEG-TS payload; the URL scheme
            // selects the transport, the muxer is the container.
            Self::Srt | Self::UdpTs => "mpegts",
            Self::Rtsp => "rtsp",
        }
    }
}

/// A sink that encodes the canvas once and pushes the *same* packet stream to a
/// remote peer over a live transport (RTMP / SRT / RTSP / MPEG-TS-over-UDP).
///
/// This is the egress twin of [`FileSink`]: identical encode-once-mux-many drive
/// loop (invariant #7), but the muxer targets a network URL instead of a file.
/// Opening the muxer **connects** to the peer, so [`PushSink::run`] only succeeds
/// when a peer is listening; with no peer it surfaces the libav connect error as
/// [`Error::Output`] rather than blocking or panicking. (CI has no peer, so the
/// run path is exercised only against a local listener; construction and muxer
/// selection are always testable.)
pub struct PushSink {
    config: EncodeConfig,
    protocol: PushProtocol,
    url: String,
}

impl PushSink {
    /// Create a push sink that will stream the encoded program to `url` using
    /// `protocol`.
    #[must_use]
    pub fn new(config: EncodeConfig, protocol: PushProtocol, url: impl Into<String>) -> Self {
        Self {
            config,
            protocol,
            url: url.into(),
        }
    }

    /// The libav muxer name this sink streams through (derived from the
    /// protocol).
    #[must_use]
    pub fn muxer_name(&self) -> &'static str {
        self.protocol.muxer_name()
    }

    /// The push protocol.
    #[must_use]
    pub const fn protocol(&self) -> PushProtocol {
        self.protocol
    }

    /// The destination URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Encode `source` once and push every packet to the remote peer.
    ///
    /// Opens a libav muxer on the URL (which connects to the peer), forcing the
    /// protocol's container muxer, then runs the shared encode-once-mux-many
    /// loop and writes the trailer.
    ///
    /// # Errors
    /// Returns [`Error::Output`] if no peer is reachable (the connect fails), or
    /// if the encoder/muxer/source errors. A push never blocks the caller
    /// waiting for a peer beyond libav's own connect.
    pub fn run<S: VideoFrameSource>(&self, source: &mut S) -> Result<EncodeStats> {
        self.config.validate()?;
        let mut encoder = VideoEncoder::new(&self.config.target()).map_err(ff)?;
        let time_base = encoder.time_base();
        // The URL is passed as a path; libav's avio resolves the scheme
        // (rtmp://, srt://, …) and the forced muxer name fixes the container.
        let mut muxer =
            Muxer::create_as(Path::new(&self.url), self.protocol.muxer_name()).map_err(ff)?;
        let stream_index = muxer
            .add_stream(encoder.as_codec_context(), time_base)
            .map_err(ff)?;
        muxer.write_header().map_err(ff)?;
        let driven = drive_to_single_muxer(
            &mut encoder,
            &mut muxer,
            stream_index,
            self.config.format,
            source,
        );
        // Finalize-on-error (see `FileSink::run`): always write the trailer
        // best-effort so a mid-run failure still leaves the transport's container
        // properly closed. `Muxer::finish` is idempotent.
        finalize_or_propagate(&mut muxer, driven)
    }
}

/// Per-run segmentation state: the open segment, completed segments, and stats.
struct SegmentState<'a> {
    dir: &'a Path,
    prefix: &'a str,
    time_base: Rational,
    /// The **single** seed encoder for the whole run. Each new segment muxer
    /// copies its codec parameters from this one opened encoder; the codec is
    /// fixed across the encode (invariant #7), so the seed is built once here —
    /// never once per segment.
    seed: VideoEncoder,
    current: Option<OpenSegment>,
    /// Completed `(path, duration_seconds)` in order.
    done: Vec<(PathBuf, f64)>,
    /// PTS of the most recently written packet (the running segment edge).
    last_pts: MediaTime,
    stats: EncodeStats,
}

/// An in-progress segment: its muxer, registered stream index, file path, and
/// the PTS of its first frame (for duration computation).
struct OpenSegment {
    muxer: Muxer,
    stream_index: usize,
    path: PathBuf,
    start_pts: MediaTime,
}

impl<'a> SegmentState<'a> {
    /// Build the per-run segmentation state, opening the **single** seed encoder
    /// here (invariant #7: one fixed codec for the whole run).
    fn new(
        config: &'a EncodeConfig,
        dir: &'a Path,
        prefix: &'a str,
        time_base: Rational,
    ) -> Result<Self> {
        let seed = VideoEncoder::new(&config.target()).map_err(ff)?;
        note_seed_encoder_built();
        Ok(Self {
            dir,
            prefix,
            time_base,
            seed,
            current: None,
            done: Vec::new(),
            last_pts: MediaTime::ZERO,
            stats: EncodeStats::default(),
        })
    }

    /// Run the encode/segment drive loop: pull frames from `source`, convert and
    /// re-stamp each (invariants #1/#3), encode once, and split the packet stream
    /// into GOP-aligned segments. On a mid-run error the caller finalizes the
    /// open segment best-effort before propagating.
    fn drive<S: VideoFrameSource>(
        &mut self,
        encoder: &mut VideoEncoder,
        converter: &mut FrameConverter,
        source: &mut S,
    ) -> Result<()> {
        let mut tick: i64 = 0;
        while let Some(frame) = source.next_frame()? {
            let prepared = converter.prepare(frame.frame, tick)?;
            encoder.send_frame(&prepared).map_err(ff)?;
            self.drain_packets(encoder)?;
            tick = tick.saturating_add(1);
        }
        encoder.send_eof().map_err(ff)?;
        self.drain_packets(encoder)
    }

    /// Drain all currently-available encoded packets from `encoder`, starting a
    /// fresh segment on each keyframe and writing each packet into the current
    /// segment. The packet type stays inferred — `receive_packet`'s output flows
    /// straight into `write_packet`.
    fn drain_packets(&mut self, encoder: &mut VideoEncoder) -> Result<()> {
        while let Some(packet) = encoder.receive_packet().map_err(ff)? {
            let is_key = packet.is_key();
            let pts = pts_from_packet(packet.pts(), self.stats.packets, self.time_base);
            if is_key {
                self.start_segment(pts)?;
            }
            let (muxer, index) = self.current_muxer()?;
            muxer.write_packet(index, packet).map_err(ff)?;
            self.record(is_key, pts);
        }
        Ok(())
    }

    /// Best-effort finalize of the currently-open segment on the error path: the
    /// open segment muxer's trailer is written (so its container is structurally
    /// closed) and the segment dropped. Any finish failure is intentionally
    /// swallowed — the original drive error is the one that propagates. Idempotent
    /// (`close_current` no-ops once `current` is taken).
    fn finalize_open_segment_best_effort(&mut self) {
        let Some(mut segment) = self.current.take() else {
            return;
        };
        // Deliberate drop of the finish result: the run is already failing, so
        // the drive error wins; we only want the bytes flushed / trailer written.
        match segment.muxer.finish() {
            Ok(()) | Err(_) => {}
        }
        note_segment_finalized();
    }

    /// Close the current segment (if any) and open a new MPEG-TS segment muxer
    /// starting at `start_pts`. Called on every keyframe so segments are
    /// GOP-aligned and each begins on a keyframe.
    fn start_segment(&mut self, start_pts: MediaTime) -> Result<()> {
        self.close_current(start_pts)?;
        let index = self.done.len();
        let path = self.dir.join(format!("{}{index}.ts", self.prefix));
        // The run's single seed encoder only seeds the muxer stream's codec
        // parameters; the actual packets all come from the shared encoder in the
        // drive loop (one *encode* — invariant #7), while each segment file is
        // its own self-contained MPEG-TS container. The seed is reused for every
        // segment — never rebuilt per segment.
        let mut muxer = Muxer::create_as(&path, "mpegts").map_err(ff)?;
        let stream_index = muxer
            .add_stream(self.seed.as_codec_context(), self.time_base)
            .map_err(ff)?;
        muxer.write_header().map_err(ff)?;
        self.current = Some(OpenSegment {
            muxer,
            stream_index,
            path,
            start_pts,
        });
        Ok(())
    }

    /// Borrow the current segment's muxer + registered stream index, so the
    /// caller can write a packet of inferred type into it.
    ///
    /// # Errors
    /// [`Error::Output`] if the encoder produced a packet before any keyframe
    /// opened a segment.
    fn current_muxer(&mut self) -> Result<(&mut Muxer, usize)> {
        let segment = self.current.as_mut().ok_or_else(|| {
            Error::Output("encoder produced a non-keyframe before any keyframe".to_owned())
        })?;
        Ok((&mut segment.muxer, segment.stream_index))
    }

    /// Record one written packet (and whether it was a keyframe) and advance the
    /// running segment edge to `pts`.
    fn record(&mut self, is_key: bool, pts: MediaTime) {
        self.last_pts = pts;
        self.stats.packets = self.stats.packets.saturating_add(1);
        if is_key {
            self.stats.keyframes = self.stats.keyframes.saturating_add(1);
        }
    }

    /// Finalize the current segment (if any), recording its duration as the gap
    /// from its start PTS to `end_pts`, bounded below by one frame.
    fn close_current(&mut self, end_pts: MediaTime) -> Result<()> {
        let Some(mut segment) = self.current.take() else {
            return Ok(());
        };
        segment.muxer.finish().map_err(ff)?;
        note_segment_finalized();
        let span_ns = end_pts
            .as_nanos()
            .saturating_sub(segment.start_pts.as_nanos());
        let frame_ns = rescale(1, self.time_base, Rational::new(1, NANOS_PER_SEC));
        let duration = seconds_from_ns(span_ns.max(frame_ns));
        self.done.push((segment.path, duration));
        Ok(())
    }

    /// Finalize the final open segment and append every segment to `playlist`.
    fn finish(&mut self, playlist: &mut MediaPlaylist, frame_ns: i64) -> Result<()> {
        let end = self
            .last_pts
            .saturating_add(MediaTime::from_nanos(frame_ns));
        self.close_current(end)?;
        for (path, duration) in &self.done {
            let uri = path
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .ok_or_else(|| Error::Output("segment path has no file name".to_owned()))?;
            playlist.push_segment(Segment::new(uri.to_owned(), *duration));
        }
        Ok(())
    }

    /// Consume the state, yielding the ordered list of segment paths written.
    fn into_segment_paths(self) -> Vec<PathBuf> {
        self.done.into_iter().map(|(path, _)| path).collect()
    }
}

/// Compute a packet's presentation time on the internal ns timeline from its
/// encoder-time-base PTS. An absent PTS falls back to the running packet count
/// (a monotonic stand-in), so durations stay sane even for codecs that omit it.
fn pts_from_packet(raw: Option<i64>, packet_count: u64, time_base: Rational) -> MediaTime {
    let ticks = raw.unwrap_or_else(|| i64::try_from(packet_count).unwrap_or(i64::MAX));
    MediaTime::from_nanos(rescale(ticks, time_base, Rational::new(1, NANOS_PER_SEC)))
}

/// Record a packet (and whether it was a keyframe) into `stats`.
fn record(stats: &mut EncodeStats, is_key: bool) {
    stats.packets = stats.packets.saturating_add(1);
    if is_key {
        stats.keyframes = stats.keyframes.saturating_add(1);
    }
}

/// Convert an integer nanosecond span into seconds (HLS `EXTINF` is decimal
/// seconds, so this float is for the text layer only — invariant #3 keeps the
/// authoritative time as integer ns).
fn seconds_from_ns(ns: i64) -> f64 {
    // `as` is banned. Split into whole seconds + remainder so each part fits an
    // i32 exactly, then recombine in f64 — exact for any non-negative ns.
    let ns = ns.max(0);
    let whole = ns / NANOS_PER_SEC;
    let frac = ns % NANOS_PER_SEC;
    // `whole` may exceed i32; saturate it through i64->f64 via integer string is
    // overkill for segment durations (seconds), so clamp to a generous i32.
    let whole_f = f64::from(i32::try_from(whole.min(i64::from(i32::MAX))).unwrap_or(i32::MAX));
    let frac_f = f64::from(i32::try_from(frac).unwrap_or(0)) / 1_000_000_000.0;
    whole_f + frac_f
}

#[cfg(all(test, feature = "ffmpeg"))]
mod tests {
    //! Unit tests using the real `ffmpeg` encode/decode path. The seed-encoder
    //! seam ([`SEED_ENCODER_BUILDS`]) is private, so this lives in-crate where it
    //! can be read directly (an integration test could not observe it).
    // Test helpers here use the test-only ergonomics the repo allows in tests
    // (the clippy.toml `allow-*-in-tests` options do not reach helper fns inside
    // a `#[cfg(test)]` module, so the relaxation is stated explicitly — matching
    // the `#![allow(...)]` header every `tests/*.rs` file carries).
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::Ordering;

    use ffmpeg_next as ffmpeg;
    use mosaic_core::time::Rational;
    use mosaic_ffmpeg::{DecodedVideoFrame, StreamVideoDecoder};

    use super::{
        EncodeConfig, Result, SegmentSink, VideoFrameSource, SEED_ENCODER_BUILDS, SEGMENT_FINALIZES,
    };
    use crate::error::Error;

    const WIDTH: u32 = 160;
    const HEIGHT: u32 = 120;

    /// Serializes the counter-based tests: the seed/finalize counters are
    /// process-global statics, and both tests exercise both counters (opening
    /// segments increments finalizes; each segment build increments seed
    /// builds), so they must not interleave or they pollute each other's reads.
    static COUNTER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn generate_clip(path: &Path, seconds: u32, fps: u32) {
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=size={WIDTH}x{HEIGHT}:rate={fps}:duration={seconds}"),
                "-pix_fmt",
                "yuv420p",
                "-c:v",
                "mpeg2video",
                "-g",
                &fps.to_string(),
                "-keyint_min",
                &fps.to_string(),
                "-sc_threshold",
                "0",
                "-f",
                "mpegts",
            ])
            .arg(path)
            .status()
            .expect("spawn ffmpeg CLI");
        assert!(status.success(), "ffmpeg CLI failed to generate the clip");
    }

    struct DecodeSource {
        input: ffmpeg::format::context::Input,
        decoder: StreamVideoDecoder,
        stream_index: usize,
        drained: bool,
    }

    impl DecodeSource {
        fn open(path: &Path) -> Self {
            let input = ffmpeg::format::input(&path).expect("open input container");
            let stream = input
                .streams()
                .best(ffmpeg::media::Type::Video)
                .expect("input has a video stream");
            let stream_index = stream.index();
            let params = stream.parameters();
            let time_base = mosaic_ffmpeg::from_ff_rational(stream.time_base());
            let decoder = StreamVideoDecoder::new(params, time_base).expect("build stream decoder");
            Self {
                input,
                decoder,
                stream_index,
                drained: false,
            }
        }
    }

    impl VideoFrameSource for DecodeSource {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
            loop {
                if let Some(frame) = self
                    .decoder
                    .receive_frame()
                    .map_err(|e| Error::Output(e.to_string()))?
                {
                    return Ok(Some(frame));
                }
                if self.drained {
                    return Ok(None);
                }
                let mut packet = ffmpeg::codec::packet::Packet::empty();
                match packet.read(&mut self.input) {
                    Ok(()) => {
                        if packet.stream() == self.stream_index {
                            self.decoder
                                .send_packet(&packet)
                                .map_err(|e| Error::Output(e.to_string()))?;
                        }
                    }
                    Err(ffmpeg::Error::Eof) => {
                        self.decoder
                            .send_eof()
                            .map_err(|e| Error::Output(e.to_string()))?;
                        self.drained = true;
                    }
                    Err(other) => return Err(Error::Output(other.to_string())),
                }
            }
        }
    }

    /// A source that yields `before_err` frames, then errors — modelling a
    /// mid-run input failure (`?` returns) with a segment muxer still open.
    struct FailAfter {
        inner: DecodeSource,
        remaining: usize,
    }

    impl VideoFrameSource for FailAfter {
        fn next_frame(&mut self) -> Result<Option<DecodedVideoFrame>> {
            if self.remaining == 0 {
                return Err(Error::Output("injected mid-run source failure".to_owned()));
            }
            self.remaining -= 1;
            self.inner.next_frame()
        }
    }

    fn config(fps: u32, gop: u32) -> EncodeConfig {
        let mut cfg = EncodeConfig::mpeg2(WIDTH, HEIGHT);
        cfg.cadence = Rational::new(i64::from(fps), 1);
        cfg.gop = gop;
        cfg
    }

    /// Count the `seg*.ts` segment files written into `dir` (one per opened
    /// segment, since [`SegmentSink`] writes each segment muxer's header on open).
    fn opened_segment_count(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension().and_then(std::ffi::OsStr::to_str) == Some("ts")
                    && p.file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .is_some_and(|n| n.starts_with("seg"))
            })
            .count()
    }

    #[test]
    fn segment_sink_finalizes_open_segment_when_source_errors_mid_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.ts");
        // 3 seconds @ 30 fps, 1-second GOP => up to 3 GOP-aligned segments.
        generate_clip(&src, 3, 30);

        let _guard = COUNTER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SEGMENT_FINALIZES.store(0, Ordering::Relaxed);
        let sink = SegmentSink::new(config(30, 30), dir.path(), "seg");
        // Yield ~2.5 GOPs so at least one full segment plus a partial one are
        // open, then fail mid-run.
        let mut source = FailAfter {
            inner: DecodeSource::open(&src),
            remaining: 75,
        };
        let err = sink
            .run(&mut source)
            .expect_err("run must surface the source error");
        assert!(
            matches!(err, Error::Output(_)),
            "the injected source error must propagate, got {err:?}"
        );

        // Every segment that was opened (header written, file on disk) must have
        // been finalized before the error propagated — including the one that was
        // still open when the source failed. The bug finished only the segments
        // closed at a later keyframe, leaving the open one un-finalized.
        let opened = opened_segment_count(dir.path());
        assert!(
            opened >= 2,
            "test needs >= 2 opened segments to be meaningful, got {opened}"
        );
        let finalized = SEGMENT_FINALIZES.load(Ordering::Relaxed);
        assert_eq!(
            finalized,
            u64::try_from(opened).expect("opened count fits u64"),
            "every opened segment must be finalized on the error path: \
             opened {opened}, finalized {finalized}"
        );
    }

    #[test]
    fn segment_sink_builds_exactly_one_seed_encoder_for_many_segments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.ts");
        // 3 seconds @ 30 fps, 1-second GOP => 3 GOP-aligned segments.
        generate_clip(&src, 3, 30);

        let _guard = COUNTER_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SEED_ENCODER_BUILDS.store(0, Ordering::Relaxed);
        let sink = SegmentSink::new(config(30, 30), dir.path(), "seg");
        let mut source = DecodeSource::open(&src);
        let result = sink.run(&mut source).expect("segment sink run");

        // Sanity: the run actually produced multiple segments (otherwise a
        // per-segment seed and a one-time seed would be indistinguishable).
        assert!(
            result.segments.len() >= 2,
            "test needs >= 2 segments to be meaningful, got {}",
            result.segments.len()
        );

        // The seed encoder (codec-parameter source for each new segment muxer)
        // must be built exactly ONCE for the whole run — never once per segment
        // (invariant #7: the codec is fixed across the encode).
        let builds = SEED_ENCODER_BUILDS.load(Ordering::Relaxed);
        assert_eq!(
            builds,
            1,
            "expected exactly one seed encoder for the whole run, got {builds} \
             across {} segments",
            result.segments.len()
        );
    }
}
