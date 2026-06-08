//! The GPU surface pool: the run-stable textures and buffers the wgpu
//! compositor reuses every tick instead of allocating per frame (EFF-0, safety
//! rule §5 / efficiency-budget §2.4 — "frame buffers come from per-device pools
//! allocated at start, never per-frame").
//!
//! Before this pool, [`crate::gpu::compositor::GpuCompositor`] created the full
//! transient set every `composite()` call — the `Rgba16Float` linear canvas
//! (~16.6 MB at 1080p), the tile texture arrays, the NV12 `y_out`/`uv_out`
//! planes, two padded readback buffers, and the uniform/storage buffers — then
//! freed them via `Drop`. At 4K25 that is ~2.6 GB/s of allocate-then-free GPU
//! churn and a real throughput limiter.
//!
//! The pool holds each of those as a cached field keyed by the dimensions that
//! size it, and **reuses** the cached resource whenever the request fits:
//!
//! - **Canvas-sized surfaces** (`canvas_lin`, `overlaid`, `y_out`, `uv_out`,
//!   the two readback buffers) are keyed on the exact `(canvas_w, canvas_h)`.
//!   The canvas size is run-stable, so a change (a resize) is rare, not
//!   per-tick: on an exact match the cached texture/buffer is returned with **no
//!   allocation**; only a genuine dimension change recreates it.
//! - **Tile arrays** (`y_array` R8, `uv_array` Rg8) are sized to the max tile
//!   dimensions across the request and always [`MAX_TILES`] layers. They grow
//!   monotonically: a request that fits the cached extent reuses it, a larger
//!   one recreates at the new (larger) extent. The shader reads each layer only
//!   over that tile's `src_w x src_h` region (fresh-written every tick via
//!   `write_texture`), so an oversized array is byte-identical — stale texels
//!   outside the written region are never sampled.
//! - **Uniform + storage buffers** (`composite`/`encode`/`overlay` uniforms,
//!   the `MAX_TILES`-sized tile-params storage buffer, the
//!   [`MAX_OVERLAY_PRIMS`]-sized overlay-prim storage buffer, the 1×1 atlas
//!   placeholder) are allocated once at their bounded maximum and refilled in
//!   place with `queue.write_buffer`/`write_texture`; the shaders read only the
//!   `count` entries actually written each tick.
//!
//! Every allocation the pool performs increments [`SurfacePool::alloc_count`],
//! the authoritative counter the EFF-0 allocation-count gate asserts is bounded
//! (one-time setup), not proportional to the tick count.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::gpu::compositor::MAX_TILES;
use crate::gpu::uniforms::{CompositeUniforms, EncodeUniforms, TileParams};

/// Whether a cached resource of `have` extent can serve a request needing
/// `need` — i.e. it is at least as large in every dimension. Pure (no GPU), so
/// the reuse/grow decision is unit-tested without an adapter.
#[must_use]
pub(crate) fn fits(have: Dim3, need: Dim3) -> bool {
    have.width >= need.width && have.height >= need.height && have.layers >= need.layers
}

/// Whether a cached canvas-sized resource keyed on `have` exactly matches the
/// requested `need`. Canvas dimensions are run-stable, so canvas-sized surfaces
/// reuse on an exact match and recreate only on a genuine resize. Pure (no GPU).
#[must_use]
pub(crate) fn exact(have: Dim2, need: Dim2) -> bool {
    have.width == need.width && have.height == need.height
}

/// A 3-D extent (texture-array: width × height × layers). Layers is 1 for a
/// plain 2-D texture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dim3 {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
    /// Array layer count.
    pub layers: u32,
}

/// A 2-D extent (width × height) keying a canvas-sized resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dim2 {
    /// Width in texels.
    pub width: u32,
    /// Height in texels.
    pub height: u32,
}

/// A pooled texture cached with the extent it was allocated for.
#[derive(Debug)]
pub(crate) struct CachedTexture {
    /// The extent the texture was created at.
    pub dim: Dim3,
    /// The texture itself.
    pub texture: wgpu::Texture,
}

/// A pooled buffer cached with the byte size it was allocated for.
#[derive(Debug)]
pub(crate) struct CachedBuffer {
    /// The size in bytes the buffer was created at.
    pub size: u64,
    /// The padded bytes-per-row used to write into / read from it (readback
    /// buffers only — `0` for storage/uniform buffers).
    pub padded_bytes_per_row: u32,
    /// The buffer itself.
    pub buffer: wgpu::Buffer,
}

/// The run-stable GPU resource pool owned by the compositor. Every field is
/// allocated lazily on first use (or first use at a given size) and reused
/// thereafter; nothing here is freed-and-reallocated per tick.
#[derive(Debug, Default)]
pub(crate) struct SurfacePool {
    /// Count of GPU `create_texture` + `create_buffer` calls the pool has made.
    /// The EFF-0 gate asserts this stops growing once the pool is warm.
    alloc_count: AtomicU64,

    // --- tile upload arrays (grow-only on dims, always MAX_TILES layers) -----
    /// Tile Y planes (`R8Unorm` texture-array).
    pub y_array: Option<CachedTexture>,
    /// Tile interleaved UV planes (`Rg8Unorm` texture-array, half-res).
    pub uv_array: Option<CachedTexture>,

    // --- canvas-sized surfaces (exact-match reuse, resize is rare) ----------
    /// Linear `Rgba16Float` composite canvas.
    pub canvas_lin: Option<CachedTexture>,
    /// Linear `Rgba16Float` overlaid canvas (overlay path only).
    pub overlaid: Option<CachedTexture>,
    /// NV12 `R8Unorm` Y output plane.
    pub y_out: Option<CachedTexture>,
    /// NV12 `Rg8Unorm` UV output plane (half-res).
    pub uv_out: Option<CachedTexture>,
    /// Padded readback buffer for the Y plane.
    pub y_readback: Option<CachedBuffer>,
    /// Padded readback buffer for the UV plane.
    pub uv_readback: Option<CachedBuffer>,

    // --- fixed-size uniform / storage buffers (allocate once, refill) -------
    /// Composite-pass uniform buffer.
    pub comp_uniform: Option<wgpu::Buffer>,
    /// Encode-pass uniform buffer.
    pub enc_uniform: Option<wgpu::Buffer>,
    /// Tile-params storage buffer, sized for [`MAX_TILES`] tiles.
    pub tile_buf: Option<wgpu::Buffer>,
    /// Overlay-pass uniform buffer.
    pub ov_uniform: Option<wgpu::Buffer>,
    /// Overlay-prim storage buffer, sized for [`MAX_OVERLAY_PRIMS`] prims.
    pub prim_buf: Option<wgpu::Buffer>,
    /// 1×1 placeholder glyph-atlas texture (the image dispatch never samples it).
    pub atlas: Option<wgpu::Texture>,
}

impl SurfacePool {
    /// The number of GPU allocations the pool has made since construction.
    /// Steady-state ticks must not increase this (EFF-0 gate).
    #[must_use]
    pub(crate) fn alloc_count(&self) -> u64 {
        self.alloc_count.load(Ordering::Relaxed)
    }

    /// Record one GPU allocation (a `create_texture` / `create_buffer`). Used
    /// only by the test counter; the slot helpers bump the counter directly
    /// (they hold `&mut Option<..>` borrows, not `&self`).
    #[cfg(test)]
    fn bump(&self) {
        self.alloc_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Reuse-or-(re)create a **grow-only** texture-array slot: keep the cached
    /// texture if it [`fits`] the request, otherwise build one at the requested
    /// extent (growing past the old one) via `make`. Returns a borrow of the
    /// resident texture. [`Option::insert`] guarantees a `Some` result, so there
    /// is no unreachable branch.
    pub(crate) fn grow_texture<'a, F>(
        slot: &'a mut Option<CachedTexture>,
        counter: &AtomicU64,
        need: Dim3,
        make: F,
    ) -> &'a wgpu::Texture
    where
        F: FnOnce(Dim3) -> wgpu::Texture,
    {
        let reuse = matches!(slot, Some(cached) if fits(cached.dim, need));
        let cached = if reuse {
            slot.get_or_insert_with(|| CachedTexture {
                dim: need,
                texture: make(need),
            })
        } else {
            // Grow to at least the request in every dimension so a subsequent
            // smaller tick reuses rather than thrashing.
            let grown = Dim3 {
                width: slot
                    .as_ref()
                    .map_or(need.width, |c| c.dim.width.max(need.width)),
                height: slot
                    .as_ref()
                    .map_or(need.height, |c| c.dim.height.max(need.height)),
                layers: slot
                    .as_ref()
                    .map_or(need.layers, |c| c.dim.layers.max(need.layers)),
            };
            counter.fetch_add(1, Ordering::Relaxed);
            slot.insert(CachedTexture {
                dim: grown,
                texture: make(grown),
            })
        };
        &cached.texture
    }

    /// Reuse-or-recreate an **exact-match** canvas-sized texture slot: keep the
    /// cached texture if its `(width, height)` equals the request, otherwise
    /// build one at the request via `make` (a rare resize). `layers` is 1 for
    /// these plain 2-D surfaces.
    pub(crate) fn exact_texture<'a, F>(
        slot: &'a mut Option<CachedTexture>,
        counter: &AtomicU64,
        need: Dim2,
        make: F,
    ) -> &'a wgpu::Texture
    where
        F: FnOnce(Dim2) -> wgpu::Texture,
    {
        let dim3 = Dim3 {
            width: need.width,
            height: need.height,
            layers: 1,
        };
        let reuse = matches!(
            slot,
            Some(cached) if exact(
                Dim2 { width: cached.dim.width, height: cached.dim.height },
                need,
            )
        );
        let cached = if reuse {
            slot.get_or_insert_with(|| CachedTexture {
                dim: dim3,
                texture: make(need),
            })
        } else {
            counter.fetch_add(1, Ordering::Relaxed);
            slot.insert(CachedTexture {
                dim: dim3,
                texture: make(need),
            })
        };
        &cached.texture
    }

    /// Reuse-or-recreate an **exact-size** buffer slot (a readback buffer keyed
    /// on its byte size + padded stride). Built via `make` on a size change.
    pub(crate) fn exact_buffer<'a, F>(
        slot: &'a mut Option<CachedBuffer>,
        counter: &AtomicU64,
        size: u64,
        padded_bytes_per_row: u32,
        make: F,
    ) -> &'a CachedBuffer
    where
        F: FnOnce(u64) -> wgpu::Buffer,
    {
        let reuse = matches!(slot, Some(cached) if cached.size == size);
        if reuse {
            slot.get_or_insert_with(|| CachedBuffer {
                size,
                padded_bytes_per_row,
                buffer: make(size),
            })
        } else {
            counter.fetch_add(1, Ordering::Relaxed);
            slot.insert(CachedBuffer {
                size,
                padded_bytes_per_row,
                buffer: make(size),
            })
        }
    }

    /// Reuse-or-create a fixed-size buffer slot allocated once and refilled in
    /// place thereafter (uniform / max-sized storage buffers). Built via `make`
    /// on first use only.
    pub(crate) fn fixed_buffer<'a, F>(
        slot: &'a mut Option<wgpu::Buffer>,
        counter: &AtomicU64,
        make: F,
    ) -> &'a wgpu::Buffer
    where
        F: FnOnce() -> wgpu::Buffer,
    {
        if slot.is_none() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        slot.get_or_insert_with(make)
    }

    /// Reuse-or-create a fixed-size texture slot allocated once (the 1×1 atlas
    /// placeholder). Built via `make` on first use only.
    pub(crate) fn fixed_texture<'a, F>(
        slot: &'a mut Option<wgpu::Texture>,
        counter: &AtomicU64,
        make: F,
    ) -> &'a wgpu::Texture
    where
        F: FnOnce() -> wgpu::Texture,
    {
        if slot.is_none() {
            counter.fetch_add(1, Ordering::Relaxed);
        }
        slot.get_or_insert_with(make)
    }

    /// A handle to the internal allocation counter, so the per-slot helpers
    /// (which take `&mut Option<..>` borrows that would otherwise conflict with
    /// `&self`) can record allocations.
    pub(crate) fn counter(&self) -> &AtomicU64 {
        &self.alloc_count
    }

    /// Bump the counter for an allocation made outside the slot helpers.
    #[cfg(test)]
    pub(crate) fn bump_for_test(&self) {
        self.bump();
    }
}

/// The byte size of the [`MAX_TILES`]-sized tile-params storage buffer.
#[must_use]
pub(crate) fn tile_buf_size() -> u64 {
    let stride = u64::try_from(core::mem::size_of::<TileParams>()).unwrap_or(0);
    stride.saturating_mul(u64::from(MAX_TILES.max(1)))
}

/// The byte size of the composite-pass uniform buffer.
#[must_use]
pub(crate) fn comp_uniform_size() -> u64 {
    u64::try_from(core::mem::size_of::<CompositeUniforms>()).unwrap_or(0)
}

/// The byte size of the encode-pass uniform buffer.
#[must_use]
pub(crate) fn enc_uniform_size() -> u64 {
    u64::try_from(core::mem::size_of::<EncodeUniforms>()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn fits_reuses_when_cached_is_large_enough() {
        let have = Dim3 {
            width: 128,
            height: 128,
            layers: MAX_TILES,
        };
        // Smaller-or-equal request fits (reuse).
        assert!(fits(
            have,
            Dim3 {
                width: 64,
                height: 64,
                layers: 4
            }
        ));
        assert!(fits(have, have));
    }

    #[test]
    fn fits_grows_when_a_dimension_exceeds_cache() {
        let have = Dim3 {
            width: 128,
            height: 128,
            layers: MAX_TILES,
        };
        // Wider than cached → must grow.
        assert!(!fits(
            have,
            Dim3 {
                width: 256,
                height: 64,
                layers: 1
            }
        ));
        // Taller than cached → must grow.
        assert!(!fits(
            have,
            Dim3 {
                width: 64,
                height: 256,
                layers: 1
            }
        ));
        // More layers than cached → must grow.
        assert!(!fits(
            have,
            Dim3 {
                width: 64,
                height: 64,
                layers: MAX_TILES + 1
            }
        ));
    }

    #[test]
    fn exact_reuses_only_on_identical_canvas_dims() {
        let have = Dim2 {
            width: 1920,
            height: 1080,
        };
        assert!(exact(have, have));
        // A different canvas (resize) must NOT reuse.
        assert!(!exact(
            have,
            Dim2 {
                width: 1280,
                height: 720
            }
        ));
        // Even a larger canvas is a resize for exact-keyed surfaces.
        assert!(!exact(
            have,
            Dim2 {
                width: 3840,
                height: 2160
            }
        ));
    }

    #[test]
    fn fixed_sizes_are_nonzero_and_bounded() {
        assert!(tile_buf_size() > 0, "tile buffer must be sized");
        assert!(
            tile_buf_size() >= comp_uniform_size(),
            "tile params dominate a single uniform"
        );
        assert!(comp_uniform_size() > 0);
        assert!(enc_uniform_size() > 0);
    }

    #[test]
    fn alloc_count_starts_at_zero_and_bumps() {
        let pool = SurfacePool::default();
        assert_eq!(pool.alloc_count(), 0);
        pool.bump_for_test();
        pool.bump_for_test();
        assert_eq!(pool.alloc_count(), 2);
    }
}
