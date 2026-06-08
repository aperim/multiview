//! wgpu device/queue acquisition that **degrades gracefully** when no GPU is
//! present.
//!
//! In this devcontainer (and many CI runners) there is no Vulkan/Metal device
//! and no `/dev/dri`, so adapter enumeration returns nothing. Per the safety
//! rules the backend must NOT panic: [`GpuContext::new`] returns
//! [`Error::NoAdapter`] / [`Error::DeviceRequest`] instead, letting callers
//! fall back to the CPU reference or skip a GPU-only test.

use crate::error::{Error, Result};

/// A headless wgpu device + queue (no surface — the compositor renders into
/// textures and reads back to NV12).
#[derive(Debug)]
pub struct GpuContext {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

impl GpuContext {
    /// Acquire a headless GPU context, trying all available backends.
    ///
    /// Returns a typed error (never panics) when no adapter or device can be
    /// obtained, which is the expected outcome on GPU-free machines.
    ///
    /// # Errors
    ///
    /// - [`Error::NoAdapter`] when no backend exposes a usable adapter.
    /// - [`Error::DeviceRequest`] when an adapter exists but a device/queue
    ///   cannot be requested (e.g. missing required features).
    pub fn new() -> Result<Self> {
        // `Instance::new`/`::default` PANICS if no backend feature is compiled
        // for this target. Guard against that explicitly so the no-GPU path
        // returns a typed error instead of unwinding (safety rule: no panics).
        if wgpu::Instance::enabled_backend_features().is_empty() {
            return Err(Error::NoAdapter(
                "no wgpu backend compiled for this target".to_owned(),
            ));
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|e| Error::NoAdapter(e.to_string()))?;

        // The encode pass writes the NV12 output planes — Y as `r8unorm`, UV as
        // `rg8unorm` — through WRITE storage textures (gpu/shaders/encode.wgsl).
        // WebGPU core does NOT guarantee `r8unorm`/`rg8unorm` as storage-texture
        // formats, so wgpu rejects the encode bind-group layout ("WriteOnly access
        // to storage textures with format R8Unorm is not supported") unless
        // `TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES` is enabled. Real desktop GPUs
        // (NVIDIA/AMD/Intel) expose it; intersect with the adapter's own features
        // so the request stays graceful on an adapter that lacks it (the GPU
        // encode path is only selected when a real adapter is present — a
        // software/llvmpipe adapter falls back to the CPU reference compositor).
        let required_features =
            wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES & adapter.features();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("multiview-compositor device"),
            required_features,
            // Request the ADAPTER's real limits, not `downlevel_defaults()`
            // (WebGL2 floor: max_texture_dimension_2d = 2048). A 4K canvas needs
            // 3840+-wide tile/output textures ("Dimension X 3840 exceeds the limit
            // of 2048"); desktop GPUs offer 16384. On a software/llvmpipe adapter
            // these limits are still ≥ default, and the GPU path only runs when a
            // real adapter was selected (else the CPU reference compositor carries
            // the program), so this never over-asks.
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| Error::DeviceRequest(e.to_string()))?;

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
        })
    }

    /// The underlying wgpu device.
    #[must_use]
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The underlying wgpu queue.
    #[must_use]
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The selected adapter (for capability/telemetry inspection).
    #[must_use]
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// The wgpu instance that owns this context.
    #[must_use]
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }
}
