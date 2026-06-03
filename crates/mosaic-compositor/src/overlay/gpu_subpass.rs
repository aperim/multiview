//! The GPU overlay sub-pass (feature `overlay` + `wgpu`): one batched compute
//! pass that blends overlay primitives premultiplied-source-over the linear
//! `Rgba16Float` canvas, between the composite and encode passes (ADR-0016
//! §4.1, invariants #5 + #8).
//!
//! There is **no GPU adapter at runtime** in this environment, so this code
//! must *compile* and the WGSL must *`naga`-validate* GPU-free
//! ([`crate::gpu::shader::validate_overlay_shader`]); the actual blend runs on
//! the CPU reference ([`crate::overlay::subpass::blend_overlays`]) in tests. The
//! two share the identical primitive model and `over` math (T7).
//!
//! Primitives are packed into one storage buffer (T5 batching); glyph coverage
//! is sampled from the persistent atlas; rects are evaluated analytically — no
//! per-frame bitmap upload (T1/T3).

use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};

use crate::error::Result;
use crate::gpu::device::GpuContext;
use crate::gpu::shader::overlay_wgsl;
use crate::overlay::subpass::{OverlayDrawList, OverlayPrimitive};

/// Hard cap on overlay primitives per frame, sizing the storage buffer.
/// Bounded by design (data-plane memory is fixed, never per-frame; ADR-E005).
pub const MAX_OVERLAY_PRIMS: u32 = 4096;

/// Primitive kind tag, shared with `overlay.wgsl`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimitiveKind {
    /// A glyph coverage quad sampling the persistent atlas.
    Glyph = 0,
    /// An analytic (optionally rounded) filled rectangle / line.
    Rect = 1,
}

impl PrimitiveKind {
    /// The numeric tag the shader switches on.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Glyph => 0,
            Self::Rect => 1,
        }
    }
}

/// One packed overlay primitive. Mirrors `OverlayPrim` in `overlay.wgsl`; every
/// field is `vec4`-aligned so the std430 storage rules hold without manual pad.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct OverlayPrimGpu {
    /// `[kind, corner_radius, atlas_x, atlas_y]`.
    pub kind_meta: [u32; 4],
    /// `[dest_x, dest_y, width, height]` in canvas pixels.
    pub rect: [i32; 4],
    /// Straight LINEAR RGBA (premultiplied in-shader by coverage).
    pub color: [f32; 4],
}

/// Overlay sub-pass uniform block. Mirrors `OverlayUniforms` in `overlay.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct OverlayUniforms {
    /// `[canvas_w, canvas_h, primitive_count, 0]`.
    pub canvas: [u32; 4],
}

impl OverlayPrimGpu {
    /// Pack a CPU [`OverlayPrimitive`] into the GPU layout.
    ///
    /// A [`OverlayPrimitive::Glyph`] carries its **atlas** location (`atlas_x`,
    /// `atlas_y`) so the shader samples coverage from the persistent atlas
    /// rather than re-uploading the bitmap (T1). [`OverlayPrimitive::FilledRect`]
    /// and [`OverlayPrimitive::Line`] are analytic.
    #[must_use]
    pub fn pack(primitive: &OverlayPrimitive, atlas_x: u32, atlas_y: u32) -> Self {
        match primitive {
            OverlayPrimitive::Glyph {
                dest_x,
                dest_y,
                width,
                height,
                color,
                ..
            } => Self {
                kind_meta: [PrimitiveKind::Glyph.as_u32(), 0, atlas_x, atlas_y],
                rect: [
                    *dest_x,
                    *dest_y,
                    i32_from_u32(*width),
                    i32_from_u32(*height),
                ],
                color: [color.r, color.g, color.b, color.a],
            },
            OverlayPrimitive::FilledRect {
                rect,
                corner_radius,
                color,
            } => Self {
                kind_meta: [PrimitiveKind::Rect.as_u32(), *corner_radius, 0, 0],
                rect: [
                    rect.x,
                    rect.y,
                    i32_from_u32(rect.width),
                    i32_from_u32(rect.height),
                ],
                color: [color.r, color.g, color.b, color.a],
            },
            OverlayPrimitive::Line { rect, color } => Self {
                kind_meta: [PrimitiveKind::Rect.as_u32(), 0, 0, 0],
                rect: [
                    rect.x,
                    rect.y,
                    i32_from_u32(rect.width),
                    i32_from_u32(rect.height),
                ],
                color: [color.r, color.g, color.b, color.a],
            },
        }
    }
}

/// The compiled overlay sub-pass: the compute pipeline + its bind-group layout.
///
/// Construct with [`OverlaySubpass::new`], which compiles + (implicitly) the
/// WGSL is naga-validated by [`crate::gpu::shader::validate_overlay_shader`].
#[derive(Debug)]
pub struct OverlaySubpass {
    pipeline: wgpu::ComputePipeline,
    layout: wgpu::BindGroupLayout,
}

impl OverlaySubpass {
    /// Compile the overlay sub-pass pipeline on an existing [`GpuContext`].
    ///
    /// # Errors
    ///
    /// Propagates a shader compile failure surfaced by the device.
    pub fn new(ctx: &GpuContext) -> Result<Self> {
        let device = ctx.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mosaic overlay subpass"),
            source: wgpu::ShaderSource::Wgsl(overlay_wgsl().into()),
        });
        let layout = bind_group_layout(device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mosaic overlay layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("mosaic overlay pipeline"),
            layout: Some(&pipeline_layout),
            module: &module,
            entry_point: Some("overlay_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            cache: None,
        });
        Ok(Self { pipeline, layout })
    }

    /// The compute pipeline (for the compositor to bind into its encoder).
    #[must_use]
    pub fn pipeline(&self) -> &wgpu::ComputePipeline {
        &self.pipeline
    }

    /// The bind-group layout (canvas-in texture, canvas-out storage, atlas,
    /// uniforms, packed primitive buffer).
    #[must_use]
    pub fn bind_group_layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    /// Pack a whole [`OverlayDrawList`] into GPU primitives, resolving each
    /// glyph's atlas slot via `atlas_slot` (returns `(x, y)` texels). Caps at
    /// [`MAX_OVERLAY_PRIMS`].
    ///
    /// Glyph primitives whose `atlas_slot` resolves to `None` (not resident) are
    /// skipped — the layer holds last-good rather than crashing (hot-path rule).
    #[must_use]
    pub fn pack_list(
        list: &OverlayDrawList,
        mut atlas_slot: impl FnMut(usize) -> Option<(u32, u32)>,
    ) -> Vec<OverlayPrimGpu> {
        let cap = usize::try_from(MAX_OVERLAY_PRIMS).unwrap_or(usize::MAX);
        let mut out = Vec::with_capacity(list.primitives.len().min(cap));
        for (i, primitive) in list.primitives.iter().enumerate() {
            if out.len() >= cap {
                break;
            }
            match primitive {
                OverlayPrimitive::Glyph { .. } => {
                    let Some((ax, ay)) = atlas_slot(i) else {
                        continue;
                    };
                    out.push(OverlayPrimGpu::pack(primitive, ax, ay));
                }
                _ => out.push(OverlayPrimGpu::pack(primitive, 0, 0)),
            }
        }
        out
    }
}

/// The overlay sub-pass bind-group layout (mirrors `overlay.wgsl` bindings).
fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("mosaic overlay bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<OverlayUniforms>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Storage { read_only: true },
                    has_dynamic_offset: false,
                    min_binding_size: min_binding_size::<OverlayPrimGpu>(),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
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

/// `min_binding_size` for a `#[repr(C)]` uniform/storage block `T`
/// (mirrors `gpu::compositor::min_binding_size`; no `as` cast).
fn min_binding_size<T>() -> Option<NonZeroU64> {
    u64::try_from(core::mem::size_of::<T>())
        .ok()
        .and_then(NonZeroU64::new)
}

/// Saturating `u32 -> i32` (overlay sizes are small), no `as` cast.
fn i32_from_u32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
