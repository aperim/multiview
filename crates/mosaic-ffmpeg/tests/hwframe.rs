//! Hardware-frame lifecycle scaffold tests.
//!
//! These run on a GPU-free machine: they assert the RAII handles are `Send`,
//! that `libav_name` mappings are stable, and that creating a device context
//! on a host with no GPU/driver fails with a **typed error** rather than
//! panicking (the leak-free, panic-safe contract). A real surface allocation is
//! GPU-only and lives in the GPU-tagged test tier, not here.
#![cfg(feature = "ffmpeg")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use mosaic_ffmpeg::{HwDeviceContext, HwDeviceKind, HwFramesContext};

#[test]
fn device_kind_names_are_stable() {
    assert_eq!(HwDeviceKind::Cuda.libav_name(), "cuda");
    assert_eq!(HwDeviceKind::Vaapi.libav_name(), "vaapi");
    assert_eq!(HwDeviceKind::Qsv.libav_name(), "qsv");
    assert_eq!(HwDeviceKind::VideoToolbox.libav_name(), "videotoolbox");
}

#[test]
fn creating_a_device_without_a_gpu_is_a_typed_error_not_a_panic() {
    // On the GPU-free CI host, creating any hardware device must surface a
    // typed error. It must NOT panic and must NOT leak — the function owns and
    // frees any partially-allocated ref internally.
    //
    // We accept either outcome defensively: an error (no device/driver — the
    // expected CI path) OR success (if a runner genuinely has the device). The
    // load-bearing assertion is "no panic, typed Result".
    for kind in [
        HwDeviceKind::Cuda,
        HwDeviceKind::Vaapi,
        HwDeviceKind::Qsv,
        HwDeviceKind::VideoToolbox,
    ] {
        match HwDeviceContext::create(kind, None) {
            Ok(device) => {
                // If a device DID open, the kind round-trips and we can drop it
                // cleanly (the Drop releases the AVBufferRef).
                assert_eq!(device.kind(), kind);
                assert!(!device.as_raw().is_null(), "live device has a non-null ref");
                drop(device);
            }
            Err(err) => {
                // Typed error rendered without panicking — the CI-green path.
                let rendered = err.to_string();
                assert!(!rendered.is_empty(), "error renders to a message");
            }
        }
    }
}

#[test]
fn hw_handles_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<HwDeviceContext>();
    assert_send::<HwFramesContext>();
}
