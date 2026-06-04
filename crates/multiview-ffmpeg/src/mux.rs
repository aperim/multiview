//! Safe muxer / output-container wrapper (the `ffmpeg` feature).
//!
//! [`Muxer`] allocates an output `AVFormatContext` for a container (mkv / mp4 /
//! mpegts), registers one stream per encoder, writes the header, accepts
//! encoded packets (rescaling their timestamps from encoder time-base into the
//! stream time-base and interleaving them), and writes the trailer. It owns the
//! context and frees it in `Drop` (via `ffmpeg_next`).
//!
//! ## Re-stamping discipline (invariants #1/#3)
//! Packets arrive stamped in the **encoder** time-base — which the encoder
//! derived from the output clock's tick counter, never from raw input PTS. The
//! muxer's only timestamp job is the mechanical encoder→stream time-base
//! rescale (libav's `av_packet_rescale_ts`); it never invents or trusts an
//! input PTS.
//! Callers must finish with [`Muxer::finish`] (writes the trailer) — dropping
//! without it leaves a container missing its trailer, so `finish` is explicit.

use std::path::Path;

use ffmpeg::{codec, format};
use ffmpeg_next as ffmpeg;

use multiview_core::time::Rational;

use crate::convert::{from_ff_rational, to_ff_rational};
use crate::decode::ensure_initialized;
use crate::error::{FfmpegError, Result};

/// A registered output stream: its index and the encoder time-base whose
/// packets feed it (the source side of the rescale into stream time-base).
#[derive(Debug, Clone, Copy)]
struct StreamInfo {
    index: usize,
    encoder_time_base: Rational,
    stream_time_base: Rational,
}

/// A safe container muxer.
///
/// `!Sync` by construction; `Send` so it can run on the egress thread.
pub struct Muxer {
    output: format::context::Output,
    streams: Vec<StreamInfo>,
    header_written: bool,
    trailer_written: bool,
}

impl Muxer {
    /// Allocate an output container for `path`, inferring the format from the
    /// extension (`.mkv`, `.mp4`, `.ts`, …).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the container cannot be allocated/opened.
    pub fn create(path: &Path) -> Result<Self> {
        ensure_initialized()?;
        let output = format::output(&path).map_err(FfmpegError::Mux)?;
        Ok(Self {
            output,
            streams: Vec::new(),
            header_written: false,
            trailer_written: false,
        })
    }

    /// Allocate an output container for `path` forcing a specific muxer by name
    /// (e.g. `"matroska"`, `"mp4"`, `"mpegts"`).
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the container cannot be allocated/opened.
    pub fn create_as(path: &Path, format_name: &str) -> Result<Self> {
        ensure_initialized()?;
        let output = format::output_as(&path, format_name).map_err(FfmpegError::Mux)?;
        Ok(Self {
            output,
            streams: Vec::new(),
            header_written: false,
            trailer_written: false,
        })
    }

    /// Register a stream from an opened encoder's codec context, recording the
    /// encoder time-base used to rescale that stream's packets. Returns the new
    /// stream index.
    ///
    /// Must be called for every stream **before** [`Muxer::write_header`].
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the container is sealed or libav refused the
    ///   stream.
    /// * [`FfmpegError::Rational`] — the encoder time-base does not fit.
    pub fn add_stream(
        &mut self,
        encoder: &codec::Context,
        encoder_time_base: Rational,
    ) -> Result<usize> {
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let tb = to_ff_rational(encoder_time_base)?;
        let index = {
            let mut stream = self
                .output
                .add_stream_with(encoder)
                .map_err(FfmpegError::Mux)?;
            stream.set_time_base(tb);
            stream.index()
        };
        // libav may adjust the stream time-base in `write_header`; the actual
        // value is re-read at header time, but seed it with the requested one.
        self.streams.push(StreamInfo {
            index,
            encoder_time_base,
            stream_time_base: encoder_time_base,
        });
        Ok(index)
    }

    /// Register a stream from a `Send` codec-parameters snapshot, recording the
    /// encoder time-base used to rescale that stream's packets. Returns the new
    /// stream index.
    ///
    /// This is the encode-once-mux-many (invariant #7, ADR-0026) sibling of
    /// [`Muxer::add_stream`]: a mux-only sink runs on its own thread with no
    /// encoder instance, so it builds its stream from a
    /// [`StreamCodecParameters`](crate::packet::StreamCodecParameters) carried
    /// across the thread boundary. Functionally identical to `add_stream` — both
    /// copy the same codec parameters onto a fresh stream — but keyed off the
    /// thread-movable snapshot rather than the live encoder context.
    ///
    /// Must be called for every stream **before** [`Muxer::write_header`].
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — the container is sealed or libav refused the
    ///   stream.
    /// * [`FfmpegError::Rational`] — the encoder time-base does not fit.
    pub fn add_stream_from_parameters(
        &mut self,
        parameters: &crate::packet::StreamCodecParameters,
        encoder_time_base: Rational,
    ) -> Result<usize> {
        if self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let tb = to_ff_rational(encoder_time_base)?;
        let index = {
            // A new stream with no codec attached, then copy the codec
            // parameters in (`avcodec_parameters_copy`) — the safe ffmpeg-next
            // path, equivalent to `add_stream_with`'s
            // `avcodec_parameters_from_context` but seeded from the snapshot.
            let mut stream = self
                .output
                .add_stream(None::<ffmpeg::Codec>)
                .map_err(FfmpegError::Mux)?;
            stream.set_parameters(parameters.as_parameters().clone());
            stream.set_time_base(tb);
            stream.index()
        };
        self.streams.push(StreamInfo {
            index,
            encoder_time_base,
            stream_time_base: encoder_time_base,
        });
        Ok(index)
    }

    /// Write the container header. Call once, after all streams are added.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the header cannot be written.
    pub fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Ok(());
        }
        self.output.write_header().map_err(FfmpegError::Mux)?;
        self.header_written = true;
        // Refresh each stream's (possibly muxer-adjusted) time-base so the
        // packet rescale targets the real value libav will use.
        for info in &mut self.streams {
            if let Some(stream) = self.output.stream(info.index) {
                info.stream_time_base = from_ff_rational(stream.time_base());
            }
        }
        Ok(())
    }

    /// Write one encoded packet to `stream_index`, rescaling its timestamps from
    /// the stream's encoder time-base into the stream time-base and interleaving
    /// it with the other streams.
    ///
    /// # Errors
    /// * [`FfmpegError::Mux`] — header not yet written, unknown stream, or a
    ///   libav write error.
    pub fn write_packet(
        &mut self,
        stream_index: usize,
        mut packet: codec::packet::Packet,
    ) -> Result<()> {
        if !self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        let info = *self
            .streams
            .iter()
            .find(|s| s.index == stream_index)
            .ok_or(FfmpegError::Mux(ffmpeg::Error::StreamNotFound))?;

        packet.set_stream(stream_index);
        let src = to_ff_rational(info.encoder_time_base)?;
        let dst = to_ff_rational(info.stream_time_base)?;
        packet.rescale_ts(src, dst);
        packet
            .write_interleaved(&mut self.output)
            .map_err(FfmpegError::Mux)
    }

    /// Write the container trailer, finalizing the file. Idempotent.
    ///
    /// # Errors
    /// Returns [`FfmpegError::Mux`] if the trailer cannot be written.
    pub fn finish(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        if !self.header_written {
            return Err(FfmpegError::Mux(ffmpeg::Error::InvalidData));
        }
        self.output.write_trailer().map_err(FfmpegError::Mux)?;
        self.trailer_written = true;
        Ok(())
    }
}
