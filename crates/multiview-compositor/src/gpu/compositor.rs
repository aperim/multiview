//! The wgpu [`GpuCompositor`]: NV12 tiles in, fixed-order pipeline on the GPU,
//! NV12 canvas out.
//!
//! Two compute passes mirror the CPU reference's two halves of the fixed color
//! order (invariant #8):
//!
//! 1. **composite** — upload each tile's NV12 (Y plane R8, interleaved UV plane
//!    Rg8) into a texture array, then per canvas pixel run range-expand ->
//!    YUV->RGB -> linearize -> primaries convert -> premultiplied-alpha blend in
//!    a linear `Rgba16Float` canvas.
//! 2. **encode** — per canvas pixel run canvas OETF -> RGB->YUV -> range
//!    compress, writing an `R8` Y plane and a half-res `Rg8` interleaved UV
//!    plane, read back as an [`Nv12Image`] (invariant #5: stays NV12).
//!
//! Tiles are placed 1:1 (no scaling) to match the CPU oracle bit-for-bit in
//! geometry; the SSIM/PSNR check covers the GPU's f32 / transcendental drift.

use std::num::NonZeroU64;

use bytemuck::Zeroable;
use multiview_core::traits::{BackendKind, Compositor};
use wgpu::util::DeviceExt;

use crate::blend::LinearRgba;
use crate::error::{Error, Result};
use crate::gpu::device::GpuContext;
use crate::gpu::shader::{composite_wgsl, encode_wgsl};
use crate::gpu::uniforms::{CompositeUniforms, EncodeUniforms, TileParams};
use crate::pipeline::{CanvasColor, Nv12Image, Tile};

/// Hard cap on tiles per composite, sizing the tile texture array and storage
/// buffer. Bounded by design (data-plane memory is fixed, never per-frame).
pub const MAX_TILES: u32 = 64;

/// Layer count of the overlay image-texture array (one per resident
/// premultiplied bitmap cue). Bounded by design (data-plane memory is fixed,
/// never per-frame; ADR-E005). Clamped to
/// [`crate::overlay::gpu_image::MAX_IMAGE_LAYERS`] by the cache.
#[cfg(feature = "overlay")]
pub const OVERLAY_IMAGE_LAYERS: u32 = 16;

/// Per-layer max extent (width = height) of the persistent overlay image
/// texture-array, allocated **once** at construction. A cue larger than this in
/// either dimension is dropped (held last-good) rather than growing the
/// allocation per frame — bounded data-plane memory (ADR-E005). 1024 covers a
/// full-width caption band at common canvas sizes.
#[cfg(feature = "overlay")]
pub const OVERLAY_IMAGE_MAX_DIM: u32 = 1024;

/// A GPU-resident NV12 multiview compositor.
///
/// Owns the device, the two compute pipelines, and their static bind-group
/// layouts; per-composite it allocates transient textures/buffers sized to the
/// request. Construct with [`GpuCompositor::new`], which fails gracefully (no
/// panic) when there is no GPU.
#[derive(Debug)]
pub struct GpuCompositor {
    ctx: GpuContext,
    composite_pipeline: wgpu::ComputePipeline,
    composite_layout: wgpu::BindGroupLayout,
    encode_pipeline_y: wgpu::ComputePipeline,
    encode_pipeline_uv: wgpu::ComputePipeline,
    encode_layout: wgpu::BindGroupLayout,
    /// The overlay sub-pass + its content-keyed image-texture cache, compiled
    /// once and reused (the cache holds the upload-once bookkeeping across
    /// ticks). Present only with the `overlay` feature.
    #[cfg(feature = "overlay")]
    overlay: std::sync::Mutex<OverlayResources>,
}

/// The overlay sub-pass, its content-keyed image-texture cache, and the
/// **persistent** `Rgba8Unorm` image texture-array the cache maps cues into —
/// all owned by the compositor so a static caption is uploaded **once** (into a
/// layer that survives across `composite_with_overlays` calls) and reused
/// thereafter (ADR-0016 upload-once; the cache `needs_upload` flag drives the
/// `write_texture`). The texture-array is allocated once at construction
/// ([`OVERLAY_IMAGE_LAYERS`] layers of [`OVERLAY_IMAGE_MAX_DIM`] px) — never
/// per frame (ADR-E005 bounded memory).
#[cfg(feature = "overlay")]
#[derive(Debug)]
struct OverlayResources {
    subpass: crate::overlay::gpu_subpass::OverlaySubpass,
    image_cache: crate::overlay::gpu_image::ImageTextureCache,
    images: wgpu::Texture,
}

impl GpuCompositor {
    /// Build a GPU compositor, acquiring a headless device and compiling both
    /// pipelines.
    ///
    /// # Errors
    ///
    /// - [`Error::NoAdapter`] / [`Error::DeviceRequest`] when no GPU is
    ///   available (the graceful-degradation path — callers fall back / skip).
    /// - [`Error::ShaderParse`] / [`Error::ShaderValidation`] if a shader is
    ///   malformed (caught by the GPU-free validator too).
    pub fn new() -> Result<Self> {
        let ctx = GpuContext::new()?;
        Self::with_context(ctx)
    }

    /// Build a GPU compositor on an already-acquired [`GpuContext`].
    ///
    /// # Errors
    ///
    /// Propagates shader/pipeline creation failures.
    pub fn with_context(ctx: GpuContext) -> Result<Self> {
        let device = ctx.device();

        let composite_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("multiview composite"),
            source: wgpu::ShaderSource::Wgsl(composite_wgsl().into()),
        });
        let encode_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("multiview encode"),
            source: wgpu::ShaderSource::Wgsl(encode_wgsl().into()),
        });

        let composite_layout = composite_bind_group_layout(device);
        let encode_layout = encode_bind_group_layout(device);

        let composite_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("multiview composite layout"),
                bind_group_layouts: &[Some(&composite_layout)],
                immediate_size: 0,
            });
        let encode_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("multiview encode layout"),
                bind_group_layouts: &[Some(&encode_layout)],
                immediate_size: 0,
            });

        let composite_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview composite pipeline"),
            layout: Some(&composite_pipeline_layout),
            module: &composite_module,
            entry_point: Some("composite_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let encode_pipeline_y = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview encode Y pipeline"),
            layout: Some(&encode_pipeline_layout),
            module: &encode_module,
            entry_point: Some("encode_y_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let encode_pipeline_uv = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("multiview encode UV pipeline"),
            layout: Some(&encode_pipeline_layout),
            module: &encode_module,
            entry_point: Some("encode_uv_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        #[cfg(feature = "overlay")]
        let overlay = {
            let subpass = crate::overlay::gpu_subpass::OverlaySubpass::new(&ctx)?;
            let image_cache =
                crate::overlay::gpu_image::ImageTextureCache::new(OVERLAY_IMAGE_LAYERS);
            // Allocate the persistent image texture-array ONCE: a static caption
            // uploaded the first tick survives in its layer across frames.
            let images = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("multiview overlay image layers"),
                size: wgpu::Extent3d {
                    width: OVERLAY_IMAGE_MAX_DIM,
                    height: OVERLAY_IMAGE_MAX_DIM,
                    depth_or_array_layers: OVERLAY_IMAGE_LAYERS.max(1),
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            std::sync::Mutex::new(OverlayResources {
                subpass,
                image_cache,
                images,
            })
        };

        Ok(Self {
            ctx,
            composite_pipeline,
            composite_layout,
            encode_pipeline_y,
            encode_pipeline_uv,
            encode_layout,
            #[cfg(feature = "overlay")]
            overlay,
        })
    }

    /// Composite a back-to-front stack of [`Tile`]s onto a `canvas_w x
    /// canvas_h` NV12 output, running the full fixed-order pipeline on the GPU.
    ///
    /// Semantics match [`crate::pipeline::composite`] (the CPU oracle): tiles
    /// are placed 1:1 in slice order, clipped to the canvas; uncovered pixels
    /// take `background` (a linear canvas-gamut color). The output carries the
    /// canvas [`CanvasColor::output_tag`].
    ///
    /// # Errors
    ///
    /// - [`Error::Geometry`] for non-even / zero canvas or tile dimensions.
    /// - [`Error::GpuLimit`] when `tiles.len()` exceeds [`MAX_TILES`].
    /// - The color `Unsupported*` / `UnresolvedColor` errors when an axis has no
    ///   shader implementation or is unresolved.
    /// - [`Error::GpuRuntime`] on a buffer-map / submission failure.
    pub fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
        // The no-overlay path: composite straight into the canvas the encode
        // pass reads, with no overlay sub-pass between (byte-for-byte the prior
        // behaviour). The overlay dispatch is `composite_with_overlays`.
        let device = self.ctx.device();
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        let canvas_lin =
            self.record_composite(&mut encoder, canvas_w, canvas_h, canvas, background, tiles)?;
        let canvas_view = canvas_lin.create_view(&wgpu::TextureViewDescriptor::default());
        self.encode_and_read(&mut encoder, &canvas_view, canvas_w, canvas_h, canvas)
    }

    /// Composite the `tiles`, blend an overlay `list` over the linear canvas via
    /// the overlay sub-pass, then encode to NV12 — the full
    /// composite → **overlay** → encode sequence (ADR-0016 §4.1, invariants
    /// #5 + #8).
    ///
    /// Image cues ([`crate::overlay::subpass::OverlayPrimitive::Image`]) are
    /// resolved against the compositor's content-keyed image-texture cache and
    /// uploaded **once** (a static caption uploaded the first tick it appears,
    /// reused thereafter), then sampled by the shader's `KIND_IMAGE` branch and
    /// blended premultiplied-over the linear canvas — matching the CPU reference
    /// [`crate::overlay::subpass::blend_overlays`] within SSIM/PSNR (never
    /// bit-exact). Analytic primitives (rect / line / stroke / ring) are blended
    /// in the same pass; glyphs are skipped here (they sample the persistent
    /// atlas the CLI bake owns — out of scope for this image-path dispatch).
    ///
    /// When `list` has no blendable primitive the overlay pass is skipped
    /// entirely and the result is byte-for-byte [`Self::composite`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::composite`], plus [`Error::GpuRuntime`] if the overlay
    /// resources lock is poisoned.
    #[cfg(feature = "overlay")]
    pub fn composite_with_overlays(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
        list: &crate::overlay::subpass::OverlayDrawList,
    ) -> Result<Nv12Image> {
        let device = self.ctx.device();
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        // Front half: composite the tiles into the straight-alpha linear canvas.
        let canvas_lin =
            self.record_composite(&mut encoder, canvas_w, canvas_h, canvas, background, tiles)?;
        let composite_view = canvas_lin.create_view(&wgpu::TextureViewDescriptor::default());

        // Plan the overlay uploads (upload-once content-keyed cache) under the
        // overlay lock; the cache state must persist across frames.
        let mut guard = self
            .overlay
            .lock()
            .map_err(|_| Error::GpuRuntime("overlay resources lock poisoned".to_owned()))?;
        let resources = &mut *guard;
        let plan = crate::overlay::gpu_image::plan_image_uploads(list, &mut resources.image_cache);

        if !plan.dispatch() {
            // No blendable overlay primitive: encode the composite canvas
            // directly (the no-overlay path — byte-for-byte `composite`).
            drop(guard);
            return self.encode_and_read(&mut encoder, &composite_view, canvas_w, canvas_h, canvas);
        }

        // Middle: record the overlay sub-pass, blending `plan` over the canvas
        // into a second linear canvas the encode pass then reads. Cues that
        // still `needs_upload` are written into the PERSISTENT image array, so a
        // static caption is uploaded once and reused.
        let overlaid = self.record_overlay(
            &mut encoder,
            resources,
            &composite_view,
            canvas_w,
            canvas_h,
            &plan,
        )?;
        drop(guard);
        let overlaid_view = overlaid.create_view(&wgpu::TextureViewDescriptor::default());

        // Back half: encode the overlaid canvas to NV12 and read it back.
        self.encode_and_read(&mut encoder, &overlaid_view, canvas_w, canvas_h, canvas)
    }

    /// Record the overlay sub-pass into `encoder`: upload each image cue's
    /// premultiplied bytes that still `needs_upload` into its resolved layer of
    /// the **persistent** `images` texture-array (upload-once — a cue resident
    /// from a prior tick keeps its bytes), upload the packed primitive buffer,
    /// build the bind group (including binding 5, the image texture-array), and
    /// dispatch the sub-pass — blending the primitives premultiplied-over the
    /// `composite_view` linear canvas into a fresh `Rgba16Float` output the
    /// encode pass reads (a read-write `rgba16float` storage texture is not
    /// portable in WebGPU core, so input and output are distinct textures;
    /// invariants #5 + #8).
    ///
    /// # Errors
    ///
    /// [`Error::Geometry`] on a primitive-count or row-stride overflow.
    #[cfg(feature = "overlay")]
    #[allow(clippy::too_many_lines)]
    // reason: the overlay sub-pass is one linear resource-setup sequence (upload
    // the cues that changed -> pack the prim/uniform buffers -> build the bind
    // group with all six bindings -> record the dispatch); splitting it would
    // scatter the transient buffer/view lifetimes that must all outlive the
    // recorded pass. Kept as one readable function with section comments,
    // mirroring `record_composite`.
    fn record_overlay(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        resources: &OverlayResources,
        composite_view: &wgpu::TextureView,
        canvas_w: u32,
        canvas_h: u32,
        plan: &crate::overlay::gpu_image::ImageUploadPlan<'_>,
    ) -> Result<wgpu::Texture> {
        use crate::overlay::gpu_subpass::{OverlayPrimGpu, OverlayUniforms};

        let device = self.ctx.device();
        let queue = self.ctx.queue();
        let subpass = &resources.subpass;
        let images = &resources.images;

        // Upload each cue that needs it into its resolved layer of the persistent
        // image array (upload-once: a cue already resident from a prior tick is
        // skipped, so a static caption is written exactly once and reused). A
        // cue larger than the bounded per-layer extent, or with a short/mismatched
        // buffer, is dropped rather than indexed/written past (hot-path rule:
        // hold last-good, never panic, never grow the allocation).
        for upload in plan.uploads() {
            if !upload.needs_upload {
                continue;
            }
            if upload.src_width == 0 || upload.src_height == 0 {
                continue;
            }
            if upload.src_width > OVERLAY_IMAGE_MAX_DIM || upload.src_height > OVERLAY_IMAGE_MAX_DIM
            {
                continue;
            }
            let expected = usize::try_from(upload.src_width)
                .ok()
                .and_then(|w| usize::try_from(upload.src_height).ok().map(|h| w * h))
                .and_then(|px| px.checked_mul(4));
            if expected != Some(upload.rgba.len()) {
                continue;
            }
            let bytes_per_row = upload
                .src_width
                .checked_mul(4)
                .ok_or_else(|| Error::Geometry("overlay image row overflow".to_owned()))?;
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: images,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: 0,
                        y: 0,
                        z: upload.layer,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                upload.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(upload.src_height),
                },
                wgpu::Extent3d {
                    width: upload.src_width,
                    height: upload.src_height,
                    depth_or_array_layers: 1,
                },
            );
        }

        // Pack the primitive count + the packed primitives into their buffers.
        let prim_count = u32::try_from(plan.prims().len())
            .map_err(|_| Error::Geometry("overlay primitive count overflow".to_owned()))?;
        let ov_uniform = OverlayUniforms {
            canvas: [canvas_w, canvas_h, prim_count, 0],
        };
        let ov_uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("multiview overlay uniforms"),
            contents: bytemuck::bytes_of(&ov_uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        // The storage binding must be non-empty even with zero packed prims
        // (e.g. only-skipped glyphs); pad with one zeroed prim the shader's
        // `count == 0` loop never reads.
        let mut prims = plan.prims().to_vec();
        if prims.is_empty() {
            prims.push(OverlayPrimGpu::zeroed());
        }
        let prim_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("multiview overlay prims"),
            contents: bytemuck::cast_slice(&prims),
            usage: wgpu::BufferUsages::STORAGE,
        });

        // The overlaid output canvas the encode pass reads (a distinct storage
        // texture — `rgba16float` read-write is not portable in WebGPU core).
        let overlaid = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview overlaid canvas"),
            size: wgpu::Extent3d {
                width: canvas_w,
                height: canvas_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        // A 1x1 placeholder glyph atlas (binding 2): glyphs are skipped by the
        // plan, so it is never sampled, but the bind group must be complete.
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview overlay atlas placeholder"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let images_view = images.create_view(&wgpu::TextureViewDescriptor::default());
        let overlaid_view = overlaid.create_view(&wgpu::TextureViewDescriptor::default());

        let overlay_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("multiview overlay bind"),
            layout: subpass.bind_group_layout(),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ov_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: prim_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(composite_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&overlaid_view),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&images_view),
                },
            ],
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("multiview overlay pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(subpass.pipeline());
            pass.set_bind_group(0, &overlay_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
        }

        Ok(overlaid)
    }

    /// Record the composite pass into `encoder`: upload the tiles and run the
    /// front half of the fixed pipeline, returning the linear `Rgba16Float`
    /// canvas texture (straight alpha) the encode (or overlay) pass then reads.
    ///
    /// # Errors
    ///
    /// Same as [`Self::composite`] (geometry, tile-count, color errors).
    #[allow(clippy::too_many_lines)]
    // reason: a GPU composite pass is one linear sequence (upload tiles ->
    // record the composite dispatch); splitting it further would scatter the
    // transient texture/buffer lifetimes and obscure the fixed pipeline order.
    // Kept as one readable function with section comments.
    fn record_composite(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<wgpu::Texture> {
        require_even_positive(canvas_w, canvas_h, "canvas")?;
        let tile_count = u32::try_from(tiles.len())
            .map_err(|_| Error::GpuLimit(format!("tile count {} overflows u32", tiles.len())))?;
        if tile_count > MAX_TILES {
            return Err(Error::GpuLimit(format!(
                "{tile_count} tiles exceeds MAX_TILES ({MAX_TILES})"
            )));
        }

        let device = self.ctx.device();
        let queue = self.ctx.queue();

        // --- upload tiles into a texture array (max dims across tiles) --------
        let max_w = tiles.iter().map(|t| t.image.width()).max().unwrap_or(2);
        let max_h = tiles.iter().map(|t| t.image.height()).max().unwrap_or(2);
        let layers = tile_count.max(1);
        let y_array = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview tile Y planes"),
            size: wgpu::Extent3d {
                width: max_w,
                height: max_h,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let uv_array = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview tile UV planes"),
            size: wgpu::Extent3d {
                width: max_w / 2,
                height: max_h / 2,
                depth_or_array_layers: layers,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let mut tile_params: Vec<TileParams> = Vec::with_capacity(tiles.len());
        for (layer, tile) in tiles.iter().enumerate() {
            let img = tile.image;
            require_even_positive(img.width(), img.height(), "tile")?;
            let layer_u32 = u32::try_from(layer)
                .map_err(|_| Error::GpuLimit("tile layer overflows u32".to_owned()))?;
            write_tile_plane(
                queue,
                &y_array,
                layer_u32,
                img.width(),
                img.height(),
                1,
                img.y_plane(),
            );
            write_tile_plane(
                queue,
                &uv_array,
                layer_u32,
                img.width() / 2,
                img.height() / 2,
                2,
                img.uv_plane(),
            );
            tile_params.push(TileParams::build(
                tile.dst_x,
                tile.dst_y,
                img.width(),
                img.height(),
                tile.opacity,
                img.color(),
                canvas,
            )?);
        }
        // The texture array must have at least one layer even with zero tiles;
        // pad the params buffer so the storage binding is non-empty.
        if tile_params.is_empty() {
            tile_params.push(TileParams::zeroed());
        }

        // --- composite pass: tiles -> linear Rgba16Float canvas --------------
        let canvas_lin = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview linear canvas"),
            size: wgpu::Extent3d {
                width: canvas_w,
                height: canvas_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });

        let comp_uniform = CompositeUniforms {
            canvas: [canvas_w, canvas_h, tile_count, 0],
            background: [background.r, background.g, background.b, background.a],
        };
        let comp_uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("multiview composite uniforms"),
            contents: bytemuck::bytes_of(&comp_uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let tile_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("multiview tile params"),
            contents: bytemuck::cast_slice(&tile_params),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let y_array_view = y_array.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_array_view = uv_array.create_view(&wgpu::TextureViewDescriptor::default());
        let canvas_lin_view = canvas_lin.create_view(&wgpu::TextureViewDescriptor::default());

        let composite_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("multiview composite bind"),
            layout: &self.composite_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: comp_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: tile_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&y_array_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&uv_array_view),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(&canvas_lin_view),
                },
            ],
        });

        // --- record the composite dispatch -----------------------------------
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("multiview composite pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.composite_pipeline);
            pass.set_bind_group(0, &composite_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
        }

        Ok(canvas_lin)
    }

    /// Record the encode pass against `canvas_view` (the straight-alpha linear
    /// canvas the encode shader reads — the composite output, or the overlaid
    /// output of [`Self::record_overlay`]), submit the `encoder`, and read back
    /// the NV12 planes (invariant #5: stays NV12).
    ///
    /// # Errors
    ///
    /// - The color `Unsupported*` / `UnresolvedColor` errors when an axis has no
    ///   shader implementation or is unresolved.
    /// - [`Error::GpuRuntime`] on a buffer-map / submission failure.
    fn encode_and_read(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        canvas_view: &wgpu::TextureView,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
    ) -> Result<Nv12Image> {
        let device = self.ctx.device();
        let queue = self.ctx.queue();

        // --- encode pass: canvas -> NV12 planes ------------------------------
        let y_out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview NV12 Y out"),
            size: wgpu::Extent3d {
                width: canvas_w,
                height: canvas_h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let uv_out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("multiview NV12 UV out"),
            size: wgpu::Extent3d {
                width: canvas_w / 2,
                height: canvas_h / 2,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        let enc_uniform = EncodeUniforms::build(canvas_w, canvas_h, canvas)?;
        let enc_uniform_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("multiview encode uniforms"),
            contents: bytemuck::bytes_of(&enc_uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let y_out_view = y_out.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_out_view = uv_out.create_view(&wgpu::TextureViewDescriptor::default());

        let encode_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("multiview encode bind"),
            layout: &self.encode_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: enc_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(canvas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&y_out_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&uv_out_view),
                },
            ],
        });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("multiview encode pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.encode_pipeline_y);
            pass.set_bind_group(0, &encode_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
            pass.set_pipeline(&self.encode_pipeline_uv);
            pass.set_bind_group(0, &encode_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w / 2, 8), div_ceil(canvas_h / 2, 8), 1);
        }

        let y_plane = self.read_plane(encoder, &y_out, canvas_w, canvas_h, 1)?;
        let uv_plane = self.read_plane(encoder, &uv_out, canvas_w / 2, canvas_h / 2, 2)?;

        // The encoder is consumed by the submit; create a fresh local move so the
        // caller's `&mut` borrow ends cleanly (we own the submission here).
        let finished = std::mem::replace(
            encoder,
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None }),
        )
        .finish();
        queue.submit(Some(finished));

        let y_bytes = self.map_read(&y_plane, canvas_w, canvas_h, 1)?;
        let uv_bytes = self.map_read(&uv_plane, canvas_w / 2, canvas_h / 2, 2)?;

        Nv12Image::new(canvas_w, canvas_h, y_bytes, uv_bytes, canvas.output_tag())
    }

    /// Stage a copy of a storage texture into a mapped-readback buffer (rows are
    /// padded to `COPY_BYTES_PER_ROW_ALIGNMENT`).
    fn read_plane(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        bytes_per_px: u32,
    ) -> Result<ReadbackBuffer> {
        let unpadded = width
            .checked_mul(bytes_per_px)
            .ok_or_else(|| Error::Geometry("readback row overflow".to_owned()))?;
        let padded = align_up(unpadded, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
        let size = u64::from(padded)
            .checked_mul(u64::from(height))
            .ok_or_else(|| Error::Geometry("readback size overflow".to_owned()))?;
        let buffer = self.ctx.device().create_buffer(&wgpu::BufferDescriptor {
            label: Some("multiview readback"),
            size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        Ok(ReadbackBuffer {
            buffer,
            padded_bytes_per_row: padded,
        })
    }

    /// Map a readback buffer and copy out the unpadded plane bytes.
    fn map_read(
        &self,
        rb: &ReadbackBuffer,
        width: u32,
        height: u32,
        bytes_per_px: u32,
    ) -> Result<Vec<u8>> {
        let slice = rb.buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |res| {
            // The receiver may be gone if the device errored; ignore send error.
            let _ = tx.send(res);
        });
        // poll until the map callback fires (headless, no surface to present).
        self.ctx
            .device()
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| Error::GpuRuntime(format!("device poll failed: {e}")))?;
        match rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(Error::GpuRuntime(format!("buffer map failed: {e}"))),
            Err(e) => return Err(Error::GpuRuntime(format!("map channel closed: {e}"))),
        }

        let row_len = usize::try_from(width)
            .ok()
            .and_then(|w| w.checked_mul(usize::try_from(bytes_per_px).ok()?))
            .ok_or_else(|| Error::Geometry("plane row overflow".to_owned()))?;
        let rows = usize::try_from(height)
            .map_err(|_| Error::Geometry("plane height overflow".to_owned()))?;
        let padded = usize::try_from(rb.padded_bytes_per_row)
            .map_err(|_| Error::Geometry("padded stride overflow".to_owned()))?;

        let data = slice.get_mapped_range();
        let mut out = Vec::with_capacity(row_len * rows);
        for row in 0..rows {
            let start = row
                .checked_mul(padded)
                .ok_or_else(|| Error::Geometry("row offset overflow".to_owned()))?;
            let end = start
                .checked_add(row_len)
                .ok_or_else(|| Error::Geometry("row end overflow".to_owned()))?;
            let chunk = data
                .get(start..end)
                .ok_or_else(|| Error::GpuRuntime("readback short row".to_owned()))?;
            out.extend_from_slice(chunk);
        }
        drop(data);
        rb.buffer.unmap();
        Ok(out)
    }
}

/// A transient readback buffer plus the padded row stride needed to strip the
/// `COPY_BYTES_PER_ROW_ALIGNMENT` padding on map.
#[derive(Debug)]
struct ReadbackBuffer {
    buffer: wgpu::Buffer,
    padded_bytes_per_row: u32,
}

impl Compositor for GpuCompositor {
    fn kind(&self) -> BackendKind {
        BackendKind::Wgpu
    }
}

/// Upload one NV12 plane into a layer of a texture array.
fn write_tile_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    layer: u32,
    width: u32,
    height: u32,
    bytes_per_px: u32,
    data: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: 0,
                y: 0,
                z: layer,
            },
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(width * bytes_per_px),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}

/// Validate even, positive dimensions (NV12 4:2:0 requires even).
fn require_even_positive(w: u32, h: u32, what: &str) -> Result<()> {
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(Error::Geometry(format!(
            "{what} dimensions must be positive and even (got {w}x{h})"
        )));
    }
    Ok(())
}

/// Ceil-divide `n` by `d` (`d > 0`), saturating (never panics / overflows for
/// the small workgroup divisors used here).
fn div_ceil(n: u32, d: u32) -> u32 {
    n.div_ceil(d.max(1))
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up(value: u32, align: u32) -> u32 {
    let a = align.max(1);
    value.div_ceil(a).saturating_mul(a)
}

/// `min_binding_size` for a `#[repr(C)]` uniform/storage block `T`.
///
/// `size_of::<T>()` is a small `usize` that fits a `u64` on every supported
/// target, so the `try_from` never fails; `NonZeroU64::new` then yields `None`
/// only for a zero-sized type (none of our blocks are). Using `try_from` keeps
/// the conversion within the `as_conversions` ban.
fn min_binding_size<T>() -> Option<NonZeroU64> {
    u64::try_from(core::mem::size_of::<T>())
        .ok()
        .and_then(NonZeroU64::new)
}

/// The composite-pass bind-group layout (mirrors `composite.wgsl` bindings).
fn composite_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("multiview composite bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<CompositeUniforms>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<TileParams>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2Array,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 4,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba16Float,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
        ],
    })
}

/// The encode-pass bind-group layout (mirrors `encode.wgsl` bindings).
fn encode_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("multiview encode bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<EncodeUniforms>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::R8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rg8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
        ],
    })
}
