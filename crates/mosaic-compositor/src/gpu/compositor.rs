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
use mosaic_core::traits::{BackendKind, Compositor};
use mosaic_core::Result as CoreResult;
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

/// A GPU-resident NV12 mosaic compositor.
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
            label: Some("mosaic composite"),
            source: wgpu::ShaderSource::Wgsl(composite_wgsl().into()),
        });
        let encode_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mosaic encode"),
            source: wgpu::ShaderSource::Wgsl(encode_wgsl().into()),
        });

        let composite_layout = composite_bind_group_layout(device);
        let encode_layout = encode_bind_group_layout(device);

        let composite_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("mosaic composite layout"),
                bind_group_layouts: &[Some(&composite_layout)],
                immediate_size: 0,
            });
        let encode_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("mosaic encode layout"),
                bind_group_layouts: &[Some(&encode_layout)],
                immediate_size: 0,
            });

        let composite_pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("mosaic composite pipeline"),
            layout: Some(&composite_pipeline_layout),
            module: &composite_module,
            entry_point: Some("composite_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let encode_pipeline_y = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("mosaic encode Y pipeline"),
            layout: Some(&encode_pipeline_layout),
            module: &encode_module,
            entry_point: Some("encode_y_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        let encode_pipeline_uv = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("mosaic encode UV pipeline"),
            layout: Some(&encode_pipeline_layout),
            module: &encode_module,
            entry_point: Some("encode_uv_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });

        Ok(Self {
            ctx,
            composite_pipeline,
            composite_layout,
            encode_pipeline_y,
            encode_pipeline_uv,
            encode_layout,
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
    #[allow(clippy::too_many_lines)]
    // reason: a GPU composite is one linear sequence (upload tiles -> composite
    // pass -> encode passes -> readback); splitting it would scatter the
    // resource lifetimes and obscure the fixed pipeline order. Kept as one
    // readable function with section comments.
    pub fn composite(
        &self,
        canvas_w: u32,
        canvas_h: u32,
        canvas: CanvasColor,
        background: LinearRgba,
        tiles: &[Tile<'_>],
    ) -> Result<Nv12Image> {
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
            label: Some("mosaic tile Y planes"),
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
            label: Some("mosaic tile UV planes"),
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
            label: Some("mosaic linear canvas"),
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
            label: Some("mosaic composite uniforms"),
            contents: bytemuck::bytes_of(&comp_uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let tile_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("mosaic tile params"),
            contents: bytemuck::cast_slice(&tile_params),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let y_array_view = y_array.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_array_view = uv_array.create_view(&wgpu::TextureViewDescriptor::default());
        let canvas_lin_view = canvas_lin.create_view(&wgpu::TextureViewDescriptor::default());

        let composite_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mosaic composite bind"),
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

        // --- encode pass: canvas -> NV12 planes ------------------------------
        let y_out = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("mosaic NV12 Y out"),
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
            label: Some("mosaic NV12 UV out"),
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
            label: Some("mosaic encode uniforms"),
            contents: bytemuck::bytes_of(&enc_uniform),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let y_out_view = y_out.create_view(&wgpu::TextureViewDescriptor::default());
        let uv_out_view = uv_out.create_view(&wgpu::TextureViewDescriptor::default());

        let encode_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mosaic encode bind"),
            layout: &self.encode_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: enc_uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&canvas_lin_view),
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

        // --- record + submit -------------------------------------------------
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("mosaic composite pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.composite_pipeline);
            pass.set_bind_group(0, &composite_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
        }
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("mosaic encode pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.encode_pipeline_y);
            pass.set_bind_group(0, &encode_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w, 8), div_ceil(canvas_h, 8), 1);
            pass.set_pipeline(&self.encode_pipeline_uv);
            pass.set_bind_group(0, &encode_bind, &[]);
            pass.dispatch_workgroups(div_ceil(canvas_w / 2, 8), div_ceil(canvas_h / 2, 8), 1);
        }

        let y_plane = self.read_plane(&mut encoder, &y_out, canvas_w, canvas_h, 1)?;
        let uv_plane = self.read_plane(&mut encoder, &uv_out, canvas_w / 2, canvas_h / 2, 2)?;
        queue.submit(Some(encoder.finish()));

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
            label: Some("mosaic readback"),
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

    fn describe_output(&self) -> CoreResult<mosaic_core::frame::FrameMeta> {
        // The render-time geometry/color is supplied per `composite` call; this
        // metadata hook is not used by the GPU path yet, so defer to the trait's
        // NotImplemented default semantics via the core error.
        Err(mosaic_core::Error::NotImplemented(
            "GpuCompositor::describe_output",
        ))
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
        label: Some("mosaic composite bgl"),
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
        label: Some("mosaic encode bgl"),
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
