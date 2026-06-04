//! Test-only **DVB-subtitle MPEG-TS fixture generator** (`test-fixtures`
//! feature, `#[doc(hidden)]`).
//!
//! The Debian/CI `FFmpeg` 7.1 CLI cannot transcode a text subtitle into a bitmap
//! `dvbsub` stream ("Subtitle encoding currently only possible from text to text
//! or bitmap to bitmap"; the `text2graphicsub` filter is not compiled), so the
//! DVB-sub caption tests build the fixture directly through libav: an in-tree
//! LGPL `mpeg2video` program plus a `dvbsub` (codec `dvb_subtitle`) bitmap
//! subtitle cue, muxed into MPEG-TS.
//!
//! This lives in `multiview-ffmpeg` (the **only** crate allowed `unsafe` for raw
//! libav* FFI) so the strictly-`forbid(unsafe)` `multiview-cli` test can call it
//! across the crate boundary; the `multiview-ffmpeg` demux test uses it too. It is
//! gated behind the off-by-default `test-fixtures` feature and `#[doc(hidden)]`
//! — it is **not** part of the product API. There is no safe `ffmpeg_next`
//! constructor for an outbound `AVSubtitle` bitmap rect, so the rect is poked
//! through the raw FFI; every `unsafe` block is bounded to libav-owned buffers
//! we allocate here and carries a `// SAFETY:` note (the crate is `unsafe =
//! deny`).

// reason: this is a libav fixture generator; raw `AVSubtitle`/`AVSubtitleRect`
// FFI has no safe `ffmpeg_next` wrapper. Every `unsafe` block has a `// SAFETY:`.
#![allow(unsafe_code)]

use std::ffi::CString;
use std::path::Path;
use std::ptr;

use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::error::{FfmpegError, Result};

/// The program canvas width of the generated clip (px).
pub const WIDTH: i32 = 640;
/// The program canvas height (px).
pub const HEIGHT: i32 = 480;
/// The output frame rate (fps).
pub const FPS: i32 = 25;
/// The clip duration (seconds).
pub const DURATION_S: i32 = 6;
/// The media second the cue becomes visible.
pub const CUE_START_S: i32 = 1;

/// The subtitle bitmap rect, in 640x480 source pixels (a wide caption band near
/// the bottom of the frame — what the per-tile burn-in must reproduce).
pub const RECT_X: i32 = 120;
/// Top of the subtitle bitmap rect (source px).
pub const RECT_Y: i32 = 380;
/// Width of the subtitle bitmap rect (source px).
pub const RECT_W: i32 = 400;
/// Height of the subtitle bitmap rect (source px).
pub const RECT_H: i32 = 60;

/// Generate the DVB-sub MPEG-TS fixture at `path`: an `mpeg2video` program plus
/// one `dvbsub` cue active from [`CUE_START_S`] (held for the decoder's default
/// on-screen duration), tagged `language=eng`.
///
/// # Errors
/// Returns [`FfmpegError`] if any libav allocation/open/encode/mux step fails.
pub fn generate_dvbsub_ts(path: &Path) -> Result<()> {
    crate::decode::ensure_initialized()?;
    // SAFETY: a single-threaded libav build sequence. We own every context /
    // stream / packet / frame we allocate below and free them before returning;
    // each inner block documents the buffers it touches.
    unsafe { generate_inner(path) }
}

/// Map a libav negative return code into a typed [`FfmpegError`].
fn ff_err(code: i32) -> FfmpegError {
    FfmpegError::Mux(ffmpeg::Error::from(code))
}

/// Bail with [`FfmpegError::Mux`] carrying `ffmpeg::Error::Bug` for a libav call
/// that returned an unexpected null/short value (no errno to map).
fn bug() -> FfmpegError {
    FfmpegError::Mux(ffmpeg::Error::Bug)
}

#[allow(clippy::too_many_lines)]
unsafe fn generate_inner(path: &Path) -> Result<()> {
    let cpath = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| bug())?;
    let cfmt = CString::new("mpegts").map_err(|_| bug())?;

    let mut oc: *mut ffi::AVFormatContext = ptr::null_mut();
    // SAFETY: out-params are a fresh null pointer + valid CStrings; libav fills
    // `oc` on success (checked) and we own it until `avformat_free_context`.
    let r = ffi::avformat_alloc_output_context2(
        &raw mut oc,
        ptr::null_mut(),
        cfmt.as_ptr(),
        cpath.as_ptr(),
    );
    if r < 0 || oc.is_null() {
        return Err(ff_err(r));
    }

    // RAII-ish: ensure we free the context on every early return below.
    let result = build_all(oc, &cpath);

    // SAFETY: `oc` is the live context we allocated; freeing it releases all
    // streams it owns. Done exactly once.
    ffi::avformat_free_context(oc);
    result
}

/// Build both streams, write the cue + the video program, and finalise. Split
/// out so the caller can free `oc` regardless of where this returns.
unsafe fn build_all(oc: *mut ffi::AVFormatContext, cpath: &CString) -> Result<()> {
    let (vst, vctx) = add_video_stream(oc)?;
    let sst = add_subtitle_stream(oc)?;

    // SAFETY: `oc` is live; `oformat` is set by alloc. Reading its `flags` is a
    // plain field read.
    let needs_file = ((*(*oc).oformat).flags & ffi::AVFMT_NOFILE) == 0;
    if needs_file {
        // SAFETY: `pb` out-param of the live context; `cpath` is a valid CString.
        let r = ffi::avio_open(&raw mut (*oc).pb, cpath.as_ptr(), ffi::AVIO_FLAG_WRITE);
        if r < 0 {
            return Err(ff_err(r));
        }
    }

    // SAFETY: header write on the live, fully-configured context.
    let r = ffi::avformat_write_header(oc, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }

    encode_subtitle_cue(oc, sst)?;
    encode_video(oc, vst, vctx)?;

    // SAFETY: trailer on the live context after a successful header + frames.
    let r = ffi::av_write_trailer(oc);
    if r < 0 {
        return Err(ff_err(r));
    }
    if needs_file {
        // SAFETY: closes + nulls the `pb` we opened above.
        ffi::avio_closep(&raw mut (*oc).pb);
    }
    // Free the encoder context we kept for the video stream (the subtitle stream
    // is configured directly on its codecpar, no context retained).
    let mut vctx_p = vctx;
    // SAFETY: `vctx` is the encoder context we allocated + opened; freed once.
    ffi::avcodec_free_context(&raw mut vctx_p);
    Ok(())
}

/// Add an `mpeg2video` stream to `oc`, returning `(stream, opened encoder ctx)`.
unsafe fn add_video_stream(
    oc: *mut ffi::AVFormatContext,
) -> Result<(*mut ffi::AVStream, *mut ffi::AVCodecContext)> {
    // SAFETY: encoder lookup by id; returns a static codec descriptor or null.
    let vcodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_MPEG2VIDEO);
    if vcodec.is_null() {
        return Err(bug());
    }
    // SAFETY: adds a stream to the live context; null on failure (checked).
    let vst = ffi::avformat_new_stream(oc, ptr::null());
    if vst.is_null() {
        return Err(bug());
    }
    // SAFETY: allocates an encoder context for the found codec; null on failure.
    let vctx = ffi::avcodec_alloc_context3(vcodec);
    if vctx.is_null() {
        return Err(bug());
    }
    // SAFETY: `vctx` is a fresh, exclusively-owned context; plain field writes.
    (*vctx).width = WIDTH;
    (*vctx).height = HEIGHT;
    (*vctx).time_base = ffi::AVRational { num: 1, den: FPS };
    (*vctx).framerate = ffi::AVRational { num: FPS, den: 1 };
    (*vctx).pix_fmt = ffi::AVPixelFormat::AV_PIX_FMT_YUV420P;
    (*vctx).gop_size = FPS;
    (*vctx).bit_rate = 2_000_000;
    // SAFETY: `oformat` flags read on the live context.
    if ((*(*oc).oformat).flags & ffi::AVFMT_GLOBALHEADER) != 0 {
        let flag = i32::try_from(ffi::AV_CODEC_FLAG_GLOBAL_HEADER).map_err(|_| bug())?;
        (*vctx).flags |= flag;
    }
    // SAFETY: opens the encoder on its own context.
    let r = ffi::avcodec_open2(vctx, vcodec, ptr::null_mut());
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: copies opened-encoder params into the stream's codecpar.
    let r = ffi::avcodec_parameters_from_context((*vst).codecpar, vctx);
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: plain field write on the owned stream.
    (*vst).time_base = (*vctx).time_base;
    Ok((vst, vctx))
}

/// Add a `dvbsub` subtitle stream to `oc`, tagged `language=eng`, returning it.
unsafe fn add_subtitle_stream(oc: *mut ffi::AVFormatContext) -> Result<*mut ffi::AVStream> {
    // SAFETY: encoder lookup by id; static descriptor or null.
    let scodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_DVB_SUBTITLE);
    if scodec.is_null() {
        return Err(bug());
    }
    // SAFETY: adds a stream to the live context; null on failure.
    let sst = ffi::avformat_new_stream(oc, ptr::null());
    if sst.is_null() {
        return Err(bug());
    }
    // SAFETY: a fresh encoder context for the dvbsub encoder; freed below.
    let sctx = ffi::avcodec_alloc_context3(scodec);
    if sctx.is_null() {
        return Err(bug());
    }
    // SAFETY: plain field writes on the fresh, owned context.
    (*sctx).width = WIDTH;
    (*sctx).height = HEIGHT;
    (*sctx).time_base = ffi::AVRational {
        num: 1,
        den: 90_000,
    };
    // SAFETY: opens the dvbsub encoder so its codecpar (the dvbsub composition
    // descriptor) is populated, then copies it into the stream.
    let r = ffi::avcodec_open2(sctx, scodec, ptr::null_mut());
    if r < 0 {
        let mut sctx_p = sctx;
        ffi::avcodec_free_context(&raw mut sctx_p);
        return Err(ff_err(r));
    }
    let r = ffi::avcodec_parameters_from_context((*sst).codecpar, sctx);
    let mut sctx_p = sctx;
    // SAFETY: free the encoder context now its params are copied to the stream.
    ffi::avcodec_free_context(&raw mut sctx_p);
    if r < 0 {
        return Err(ff_err(r));
    }
    // SAFETY: plain field write + a metadata dict set on the owned stream.
    (*sst).time_base = ffi::AVRational {
        num: 1,
        den: 90_000,
    };
    let key = CString::new("language").map_err(|_| bug())?;
    let val = CString::new("eng").map_err(|_| bug())?;
    ffi::av_dict_set(&raw mut (*sst).metadata, key.as_ptr(), val.as_ptr(), 0);
    Ok(sst)
}

/// Encode one PAL8 bitmap DVB-sub cue (a solid white band) and mux it.
unsafe fn encode_subtitle_cue(
    oc: *mut ffi::AVFormatContext,
    sst: *mut ffi::AVStream,
) -> Result<()> {
    // The subtitle stream's encoder context was freed after configuring the
    // stream; build a fresh one to encode the cue (libav allows this — the
    // encoder is stateless per call for dvbsub).
    let scodec = ffi::avcodec_find_encoder(ffi::AVCodecID::AV_CODEC_ID_DVB_SUBTITLE);
    if scodec.is_null() {
        return Err(bug());
    }
    let sctx = ffi::avcodec_alloc_context3(scodec);
    if sctx.is_null() {
        return Err(bug());
    }
    // SAFETY: fresh owned context; field writes then open.
    (*sctx).width = WIDTH;
    (*sctx).height = HEIGHT;
    (*sctx).time_base = ffi::AVRational {
        num: 1,
        den: 90_000,
    };
    let r = ffi::avcodec_open2(sctx, scodec, ptr::null_mut());
    if r < 0 {
        let mut sctx_p = sctx;
        ffi::avcodec_free_context(&raw mut sctx_p);
        return Err(ff_err(r));
    }

    let res = encode_and_write_cue(oc, sst, sctx);
    let mut sctx_p = sctx;
    // SAFETY: free the cue-encoder context exactly once.
    ffi::avcodec_free_context(&raw mut sctx_p);
    res
}

/// Build the `AVSubtitle` with one PAL8 bitmap rect, encode it on `sctx`, and
/// mux the resulting packet on `sst`.
unsafe fn encode_and_write_cue(
    oc: *mut ffi::AVFormatContext,
    sst: *mut ffi::AVStream,
    sctx: *mut ffi::AVCodecContext,
) -> Result<()> {
    let area =
        usize::try_from(RECT_W).map_err(|_| bug())? * usize::try_from(RECT_H).map_err(|_| bug())?;
    // Every pixel -> palette index 1 (opaque white); index 0 is transparent.
    let mut indices = vec![1u8; area];
    // RGB32 palette (native-endian u32; B,G,R,A on a little-endian host).
    let mut palette = vec![0u32; 256];
    if let Some(slot) = palette.get_mut(1) {
        *slot = 0xFFFF_FFFF;
    }

    let mut rect = ffi::AVSubtitleRect {
        x: RECT_X,
        y: RECT_Y,
        w: RECT_W,
        h: RECT_H,
        nb_colors: 256,
        data: [ptr::null_mut(); 4],
        linesize: [0; 4],
        flags: 0,
        type_: ffi::AVSubtitleType::SUBTITLE_BITMAP,
        text: ptr::null_mut(),
        ass: ptr::null_mut(),
    };
    // SAFETY: `indices`/`palette` outlive the encode call below; the encoder
    // reads `data[0]` as `linesize[0] * h` PAL8 bytes and `data[1]` as the CLUT.
    rect.data[0] = indices.as_mut_ptr();
    rect.linesize[0] = RECT_W;
    rect.data[1] = palette.as_mut_ptr().cast::<u8>();
    rect.linesize[1] = 256 * 4;

    let mut rect_ptr: *mut ffi::AVSubtitleRect = &raw mut rect;
    // On-screen hold (ms) the encoder records. The `dvbsub` decoder ignores this
    // and applies its own default on-screen duration, but a non-zero value keeps
    // the encoded segment well-formed. 3 s.
    let hold_ms: u32 = 3000;
    let sub = ffi::AVSubtitle {
        format: 0, // graphics/bitmap
        start_display_time: 0,
        end_display_time: hold_ms,
        num_rects: 1,
        rects: &raw mut rect_ptr,
        pts: i64::from(CUE_START_S) * 1_000_000, // AV_TIME_BASE (microseconds)
    };

    let mut buf = vec![0u8; 1 << 20];
    let cap = i32::try_from(buf.len()).map_err(|_| bug())?;
    // SAFETY: `buf` is a writable owned slice of `cap` bytes; `sub` + its rect
    // are fully populated and outlive the call.
    let n = ffi::avcodec_encode_subtitle(sctx, buf.as_mut_ptr(), cap, &raw const sub);
    if n <= 0 {
        return Err(ff_err(n));
    }

    // SAFETY: allocate a packet sized to the encoded bytes, copy them in.
    let pkt = ffi::av_packet_alloc();
    if pkt.is_null() {
        return Err(bug());
    }
    let r = ffi::av_new_packet(pkt, n);
    if r < 0 {
        let mut pkt_p = pkt;
        ffi::av_packet_free(&raw mut pkt_p);
        return Err(ff_err(r));
    }
    let len = usize::try_from(n).map_err(|_| bug())?;
    // SAFETY: `pkt.data` is `n` bytes (just allocated); `buf` holds `>= n` bytes.
    ptr::copy_nonoverlapping(buf.as_ptr(), (*pkt).data, len);
    (*pkt).stream_index = (*sst).index;
    let pts = i64::from(CUE_START_S) * 90_000;
    (*pkt).pts = pts;
    (*pkt).dts = pts;
    (*pkt).duration = i64::from(hold_ms) * 90; // ms -> 90 kHz ticks
                                               // SAFETY: interleaved write on the live container; libav takes the packet.
    let r = ffi::av_interleaved_write_frame(oc, pkt);
    let mut pkt_p = pkt;
    ffi::av_packet_free(&raw mut pkt_p);
    if r < 0 {
        return Err(ff_err(r));
    }
    Ok(())
}

/// Encode `DURATION_S * FPS` grey video frames and mux them.
unsafe fn encode_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
) -> Result<()> {
    let w = u32::try_from(WIDTH).map_err(|_| bug())?;
    let h = u32::try_from(HEIGHT).map_err(|_| bug())?;
    // Use `ffmpeg_next`'s safe `frame::Video` so the pixel-format discriminant +
    // backing-buffer allocation are handled without an `as`-cast on the C enum.
    let mut video = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);

    let total = DURATION_S * FPS;
    for i in 0..i64::from(total) {
        // SAFETY: `video.as_mut_ptr()` is the live owned frame; we fill its
        // planes via the raw pointers (the safe API has no constant-fill).
        let frame = video.as_mut_ptr();
        let r = ffi::av_frame_make_writable(frame);
        if r < 0 {
            return Err(ff_err(r));
        }
        // A dim grey field: Y ~ 40, neutral chroma 128.
        fill_plane((*frame).data[0], (*frame).linesize[0], WIDTH, HEIGHT, 40);
        fill_plane(
            (*frame).data[1],
            (*frame).linesize[1],
            WIDTH / 2,
            HEIGHT / 2,
            128,
        );
        fill_plane(
            (*frame).data[2],
            (*frame).linesize[2],
            WIDTH / 2,
            HEIGHT / 2,
            128,
        );
        (*frame).pts = i;
        drain_video(oc, vst, vctx, frame)?;
    }
    // Flush.
    let r = ffi::avcodec_send_frame(vctx, ptr::null());
    if r < 0 {
        return Err(ff_err(r));
    }
    drain_video(oc, vst, vctx, ptr::null_mut())
}

/// Send `frame` (or null to flush) and mux every packet it yields.
unsafe fn drain_video(
    oc: *mut ffi::AVFormatContext,
    vst: *mut ffi::AVStream,
    vctx: *mut ffi::AVCodecContext,
    frame: *mut ffi::AVFrame,
) -> Result<()> {
    if !frame.is_null() {
        // SAFETY: send a valid frame to the opened encoder.
        let r = ffi::avcodec_send_frame(vctx, frame);
        if r < 0 {
            return Err(ff_err(r));
        }
    }
    let pkt = ffi::av_packet_alloc();
    if pkt.is_null() {
        return Err(bug());
    }
    let result = (|| -> Result<()> {
        loop {
            // SAFETY: drains buffered packets from the encoder into `pkt`.
            let r = ffi::avcodec_receive_packet(vctx, pkt);
            if r == ffi::AVERROR(ffi::EAGAIN) || r == ffi::AVERROR_EOF {
                return Ok(());
            }
            if r < 0 {
                return Err(ff_err(r));
            }
            (*pkt).stream_index = (*vst).index;
            ffi::av_packet_rescale_ts(pkt, (*vctx).time_base, (*vst).time_base);
            let r = ffi::av_interleaved_write_frame(oc, pkt);
            if r < 0 {
                return Err(ff_err(r));
            }
        }
    })();
    let mut pkt_p = pkt;
    // SAFETY: free the drain packet exactly once.
    ffi::av_packet_free(&raw mut pkt_p);
    result
}

/// Fill a single plane with a constant byte (`linesize` row stride).
unsafe fn fill_plane(data: *mut u8, linesize: i32, w: i32, h: i32, val: u8) {
    if data.is_null() || w <= 0 || h <= 0 {
        return;
    }
    let Ok(width) = usize::try_from(w) else {
        return;
    };
    for row in 0..h {
        let Ok(stride) = isize::try_from(i64::from(row) * i64::from(linesize)) else {
            return;
        };
        // SAFETY: row<h and `stride` is within the plane allocated by libav at
        // `linesize * h`; we write `width <= linesize` bytes per row.
        let line = data.offset(stride);
        ptr::write_bytes(line, val, width);
    }
}
