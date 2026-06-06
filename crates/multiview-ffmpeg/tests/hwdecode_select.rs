//! Hardware-decoder NAME resolution tests (the `cuda`/`ffmpeg` features).
//!
//! These do NOT open a GPU: they only prove the logical-codec -> concrete libav
//! `*_cuvid` decoder-name mapping is correct and that name actually resolves in
//! the linked libav registry (the cuvid *wrapper* decoders are registered even
//! on a GPU-free box; only *opening* one needs a device). Selection on a build
//! WITHOUT the `cuda` feature must yield no hardware candidate, so the LGPL/
//! software default can never silently reach NVDEC.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use multiview_ffmpeg::hwdecode::{cuvid_decoder, HwInputCodec};

#[test]
fn cuvid_names_follow_the_libav_convention() {
    // The pure name mapping is always available (feature-independent); the names
    // must match libav's registered `*_cuvid` wrappers, verified by name exactly
    // like the encoder candidate list.
    assert_eq!(HwInputCodec::H264.cuvid_name(), Some("h264_cuvid"));
    assert_eq!(HwInputCodec::H265.cuvid_name(), Some("hevc_cuvid"));
    assert_eq!(HwInputCodec::Av1.cuvid_name(), Some("av1_cuvid"));
    assert_eq!(HwInputCodec::Vp9.cuvid_name(), Some("vp9_cuvid"));
    assert_eq!(HwInputCodec::Mpeg2Video.cuvid_name(), Some("mpeg2_cuvid"));
}

#[cfg(feature = "cuda")]
#[test]
fn cuvid_decoder_offers_the_nvdec_name_only_with_cuda() {
    // The feature-gated `cuvid_decoder` selector returns the NVDEC name when the
    // `cuda` feature is compiled — the decode-side analogue of `nvenc_encoder`.
    assert_eq!(cuvid_decoder(HwInputCodec::H264), Some("h264_cuvid"));
    assert_eq!(cuvid_decoder(HwInputCodec::H265), Some("hevc_cuvid"));
}

#[cfg(not(feature = "cuda"))]
#[test]
fn cuvid_decoder_offers_nothing_without_cuda() {
    // Without `cuda`, the selector names no NVDEC decoder: the LGPL/software
    // default can never silently reach NVDEC (matches the encoder-side policy).
    assert_eq!(cuvid_decoder(HwInputCodec::H264), None);
    assert_eq!(cuvid_decoder(HwInputCodec::H265), None);
}

// The candidate-list / availability-probe seam needs the `ffmpeg` feature (it
// queries the linked libav registry). Behind `cuda` the NVDEC wrapper name is a
// candidate; without `cuda` it never is.
#[cfg(all(feature = "ffmpeg", feature = "cuda"))]
#[test]
fn cuvid_wrapper_names_resolve_in_the_linked_libav_registry() {
    use multiview_ffmpeg::hwdecode::select_decoder;
    // The cuvid wrapper decoders are registered even with no GPU present, so the
    // name resolves here; this proves the mapping is not aspirational. (Opening
    // one would need a device — that is the GPU-runner test.)
    assert_eq!(
        select_decoder(HwInputCodec::H264, true),
        Some("h264_cuvid"),
        "h264_cuvid must be registered in the linked FFmpeg 7.1 build"
    );
    assert_eq!(select_decoder(HwInputCodec::H265, true), Some("hevc_cuvid"));
}

#[cfg(all(feature = "ffmpeg", not(feature = "cuda")))]
#[test]
fn without_cuda_no_hardware_decoder_is_selected() {
    use multiview_ffmpeg::hwdecode::select_decoder;
    // Asking for a hardware decoder on a build without `cuda` returns None: the
    // LGPL/software default never reaches NVDEC.
    assert_eq!(select_decoder(HwInputCodec::H264, true), None);
    assert_eq!(select_decoder(HwInputCodec::H265, true), None);
}
