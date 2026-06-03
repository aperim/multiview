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

        let mut tick: i64 = 0;
        let mut stats = EncodeStats::default();
        let mut converter = FrameConverter::new(self.config.format);
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
        muxer.finish().map_err(ff)?;
        Ok(stats)
    }

    /// The path this sink writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
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

        let mut state = SegmentState::new(&self.config, &self.dir, &self.prefix, time_base);
        let mut converter = FrameConverter::new(self.config.format);
        let mut tick: i64 = 0;
        while let Some(frame) = source.next_frame()? {
            let prepared = converter.prepare(frame.frame, tick)?;
            encoder.send_frame(&prepared).map_err(ff)?;
            while let Some(packet) = encoder.receive_packet().map_err(ff)? {
                let is_key = packet.is_key();
                let pts = pts_from_packet(packet.pts(), state.stats.packets, time_base);
                // Start a fresh segment on each keyframe, then write the packet
                // into the current segment. The packet type stays inferred —
                // `receive_packet`'s output flows straight into `write_packet`.
                if is_key {
                    state.start_segment(pts)?;
                }
                let (muxer, index) = state.current_muxer()?;
                muxer.write_packet(index, packet).map_err(ff)?;
                state.record(is_key, pts);
            }
            tick = tick.saturating_add(1);
        }
        encoder.send_eof().map_err(ff)?;
        while let Some(packet) = encoder.receive_packet().map_err(ff)? {
            let is_key = packet.is_key();
            let pts = pts_from_packet(packet.pts(), state.stats.packets, time_base);
            if is_key {
                state.start_segment(pts)?;
            }
            let (muxer, index) = state.current_muxer()?;
            muxer.write_packet(index, packet).map_err(ff)?;
            state.record(is_key, pts);
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

/// Per-run segmentation state: the open segment, completed segments, and stats.
struct SegmentState<'a> {
    config: &'a EncodeConfig,
    dir: &'a Path,
    prefix: &'a str,
    time_base: Rational,
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
    fn new(config: &'a EncodeConfig, dir: &'a Path, prefix: &'a str, time_base: Rational) -> Self {
        Self {
            config,
            dir,
            prefix,
            time_base,
            current: None,
            done: Vec::new(),
            last_pts: MediaTime::ZERO,
            stats: EncodeStats::default(),
        }
    }

    /// Close the current segment (if any) and open a new MPEG-TS segment muxer
    /// starting at `start_pts`. Called on every keyframe so segments are
    /// GOP-aligned and each begins on a keyframe.
    fn start_segment(&mut self, start_pts: MediaTime) -> Result<()> {
        self.close_current(start_pts)?;
        let index = self.done.len();
        let path = self.dir.join(format!("{}{index}.ts", self.prefix));
        // A throwaway encoder context only seeds the muxer stream's codec
        // parameters; the actual packets all come from the single shared
        // encoder in the drive loop (one *encode* — invariant #7), while each
        // segment file is its own self-contained MPEG-TS container.
        let seed = VideoEncoder::new(&self.config.target()).map_err(ff)?;
        let mut muxer = Muxer::create_as(&path, "mpegts").map_err(ff)?;
        let stream_index = muxer
            .add_stream(seed.as_codec_context(), self.time_base)
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
