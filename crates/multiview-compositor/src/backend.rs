//! The compositor **backend-selection seam** (invariant #1: degradation-safe).
//!
//! The run path composites once per output tick. By default it uses the
//! pure-Rust CPU reference ([`crate::pipeline::composite`]); under the off-by-default
//! `wgpu` feature it may *prefer* the portable GPU compositor (`gpu::GpuCompositor`,
//! a plain code span: the type is absent from the default no-`wgpu` doc build).
//! This module is the single seam where that choice is made.
//!
//! **The selection is degradation-safe.** A missing or failed GPU must never
//! stall or crash the run (invariant #1, safety rule §3): [`RunBackend::select`]
//! tries the GPU only when one is *preferred* AND the `wgpu` feature is
//! compiled, and on **any** GPU initialization error (no adapter, device
//! request failure, shader build failure) it **falls back to the CPU
//! reference** rather than propagating the error or panicking. The CPU path is
//! the default for both `select(false)` and a build without the feature.
//!
//! Once constructed, [`RunBackend::composite`] dispatches to the chosen
//! path with the exact signature/semantics of [`crate::pipeline::composite`]: tiles
//! placed 1:1 in slice order, clipped to the canvas; uncovered pixels take
//! `background`; the output carries the canvas tag. The CPU variant is a thin,
//! byte-for-byte dispatch to the free function (no re-encode, no reorder).
//!
//! Neither variant blocks on an input or a client. The GPU `composite` is a
//! synchronous submit+readback that **returns** a typed error on a runtime
//! failure (it does not wait forever); callers on the hot path treat such an
//! error as a per-tick fault and hold last-good rather than crashing.

use crate::blend::LinearRgba;
use crate::error::Result;
use crate::pipeline::{composite as cpu_composite, CanvasColor, Nv12Image, Tile};

/// Which compositor backend a [`RunBackend`] resolved to.
///
/// `Cpu` is always available (the pure-Rust reference). `Gpu` exists only when
/// the crate is built with the `wgpu` feature *and* an adapter initialized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RunBackendKind {
    /// The pure-Rust CPU reference compositor ([`crate::pipeline::composite`]).
    Cpu,
    /// The portable wgpu GPU compositor ([`crate::gpu::GpuCompositor`]).
    #[cfg(feature = "wgpu")]
    Gpu,
}

/// A constructed compositor backend: the CPU reference, or (under `wgpu` with a
/// live adapter) the GPU compositor.
///
/// Build one with [`RunBackend::cpu`] (always CPU) or
/// [`RunBackend::select`] (GPU-preferred with CPU fallback). Dispatch a
/// per-tick composite through [`RunBackend::composite`].
#[derive(Debug)]
#[non_exhaustive]
pub enum RunBackend {
    /// The pure-Rust CPU reference compositor. Always available.
    Cpu,
    /// The wgpu GPU compositor, holding its initialized device/pipelines.
    #[cfg(feature = "wgpu")]
    Gpu(Box<crate::gpu::GpuCompositor>),
}

impl RunBackend {
    /// The pure-Rust CPU reference backend. Always available, never fails.
    #[must_use]
    pub const fn cpu() -> Self {
        Self::Cpu
    }

    /// Select a backend, optionally preferring the GPU.
    ///
    /// When `prefer_gpu` is `true` **and** the `wgpu` feature is compiled, this
    /// attempts to acquire the GPU compositor. On success it returns the GPU
    /// backend; on **any** initialization error (no adapter, device-request
    /// failure, shader build failure) it logs the reason at `info` and **falls
    /// back to the CPU reference** — the degradation-safe path (invariant #1: a
    /// missing/failed GPU never stalls or crashes the run). When `prefer_gpu`
    /// is `false`, or the feature is off, it returns the CPU backend directly.
    ///
    /// This function never panics and never returns an error: the result is
    /// always a usable backend.
    #[must_use]
    pub fn select(prefer_gpu: bool) -> Self {
        if prefer_gpu {
            #[cfg(feature = "wgpu")]
            {
                match crate::gpu::GpuCompositor::new() {
                    Ok(gpu) => {
                        tracing::info!("compositor backend: GPU (wgpu) selected");
                        return Self::Gpu(Box::new(gpu));
                    }
                    Err(e) => {
                        tracing::info!(
                            error = %e,
                            "GPU compositor unavailable; falling back to CPU reference"
                        );
                    }
                }
            }
            #[cfg(not(feature = "wgpu"))]
            {
                tracing::debug!(
                    "GPU compositor requested but the `wgpu` feature is not compiled; using CPU"
                );
            }
        }
        Self::Cpu
    }

    /// The backend this resolved to (for telemetry / introspection).
    #[must_use]
    pub const fn kind(&self) -> RunBackendKind {
        match self {
            Self::Cpu => RunBackendKind::Cpu,
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => RunBackendKind::Gpu,
        }
    }

    /// `true` if this backend runs the composite on the GPU.
    #[must_use]
    pub const fn is_gpu(&self) -> bool {
        match self {
            Self::Cpu => false,
            #[cfg(feature = "wgpu")]
            Self::Gpu(_) => true,
        }
    }

    /// Composite one back-to-front stack of [`Tile`]s onto a `canvas_w x
    /// canvas_h` NV12 output, dispatching to the chosen backend.
    ///
    /// Semantics are identical to [`crate::pipeline::composite`] on both paths: tiles
    /// placed 1:1 in slice order, clipped to the canvas; uncovered pixels take
    /// `background` (a linear canvas-gamut color); the output carries the canvas
    /// tag. The CPU variant is byte-for-byte the free function; the GPU variant
    /// matches it within an SSIM/PSNR threshold (GPU is never bit-exact).
    ///
    /// This call is synchronous and **never blocks on an input or a client**.
    /// On the GPU path a submit/readback failure surfaces as a typed
    /// [`crate::error::Error`] (it does not wait forever), so a hot-loop caller
    /// can hold last-good on error rather than stalling the output clock
    /// (invariant #1, safety rule §3).
    ///
    /// # Errors
    ///
    /// Propagates the compositor [`crate::error::Error`] for a structurally
    /// invalid request (odd/zero canvas, unresolved/unsupported color axis) or,
    /// on the GPU path, a `GpuLimit`/`GpuRuntime` failure.
    pub fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        match self {
            Self::Cpu => cpu_composite(canvas_w, canvas_h, canvas, background, tiles),
            #[cfg(feature = "wgpu")]
            Self::Gpu(gpu) => gpu.composite(canvas_w, canvas_h, canvas, background, tiles),
        }
    }
}
