//! # mosaic-compositor
//!
//! The custom compositor: scale + place + per-tile color convert +
//! linear-light blend + overlay compositing. The library target is
//! `mosaic_compositor`.
//!
//! This crate owns the **color math of invariant #8** as pure, exact,
//! unit-tested Rust, plus a **CPU reference compositor** that runs the math in
//! the one canonical order (color-management.md §2 / ADR-C003), which must
//! never be reordered:
//!
//! detect 4 axes -> range expand -> YUV->RGB matrix -> linearize (EOTF) ->
//! primaries convert (in linear) -> scale + premultiplied-alpha blend (in
//! linear) -> OETF -> RGB->YUV + range compress -> tag output.
//!
//! The default build is **pure Rust** (no GPU, no native libraries): the CPU
//! reference is the golden-frame oracle (bit-exact on CPU). The GPU/wgpu path
//! lives in the `gpu` module behind the off-by-default `wgpu` feature; it runs the
//! **same** fixed pipeline in WGSL and matches the CPU oracle within an
//! SSIM/PSNR threshold (GPU is never bit-exact). Vendor fast paths
//! (`cuda`/`metal`/`vaapi`) remain out of scope for this module.
//!
//! ## Module map
//!
//! - [`range`] — limited<->full quantization range expand/compress (8-bit).
//! - [`matrix`] — YUV'<->R'G'B' matrices (BT.601/709/2020-NCL).
//! - [`transfer`] — EOTF/OETF (sRGB, BT.709/BT.1886, PQ ST 2084, HLG B67).
//! - [`primaries`] — linear-light gamut conversion (709<->2020 via XYZ).
//! - [`blend`] — premultiplied-alpha source-over in linear light.
//! - [`pipeline`] — the fixed-order pipeline + CPU reference compositor.
//! - `gpu` *(feature `wgpu`)* — the portable GPU compositor + WGSL shaders
//!   (named as a plain code span: the module is absent from the default doc build).
//! - [`error`] — the per-crate [`Error`] taxonomy.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod blend;
pub mod error;
#[cfg(feature = "wgpu")]
pub mod gpu;
pub mod matrix;
pub mod pipeline;
pub mod primaries;
pub mod range;
pub mod transfer;

pub use error::{Error, Result};
pub use pipeline::{
    canvas_linear_to_output_yuv, composite, tile_yuv_to_canvas_linear, CanvasColor, Nv12Image, Tile,
};

#[cfg(feature = "wgpu")]
pub use gpu::{GpuCompositor, GpuContext};
