use wgpu::{CommandEncoder, Device, Queue, Sampler};
use std::cell::RefCell;

pub struct GpuContext {
    pub device: Device,
    pub queue: Queue,
    pub linear_sampler: Sampler,
    /// Per-frame encoder. Created in begin_frame(), submitted in end_frame().
    encoder: RefCell<Option<CommandEncoder>>,
}

/// The backend the beauty chain runs on, pinned per platform.
///
/// Not a preference, and `Backends::all()` is what this replaced. wgpu
/// enumerates Vulkan first on Windows, so "all" lands the chain on Intel's
/// Vulkan ICD (`igvk64.dll`, driver 30.0.101.1339), which access-violates
/// while the beauty pipelines are being created and takes the whole process
/// down. The host pins DX12 on Windows as well (for the Preview's shared
/// textures, see `reaction::lib::pin_windows_backend`); the beauty chain has
/// no interop of its own, but DX12 is the one backend every machine that can
/// run this program's D3D11 compositor already has. macOS and Linux follow
/// the host's pins for the same reason.
fn backends() -> wgpu::Backends {
    #[cfg(target_os = "windows")]
    let backends = wgpu::Backends::DX12;
    #[cfg(target_os = "macos")]
    let backends = wgpu::Backends::METAL;
    #[cfg(target_os = "linux")]
    let backends = wgpu::Backends::VULKAN;
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    let backends = wgpu::Backends::all();
    backends
}

impl GpuContext {
    pub fn new() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: backends(),
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: None,
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .expect("Failed to find suitable GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("gpupixel-rs device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::default(),
            },
        ))
        .expect("Failed to create device");

        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shared linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });

        Self {
            device,
            queue,
            linear_sampler,
            encoder: RefCell::new(None),
        }
    }

    /// 用外部传入的 device/queue 构造(与其它使用者共享同一 GPU 队列,消除多 wgpu 设备跨队列争抢)。
    /// Device/Queue 是 Arc-backed,clone 廉价且指向同一底层;本 GpuContext 仍持自己的 encoder
    /// RefCell(线程本地),故各使用线程各建一个 GpuContext wrap 同一共享 device/queue。
    pub fn from_device(device: Device, queue: Queue) -> Self {
        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shared linear sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        Self {
            device,
            queue,
            linear_sampler,
            encoder: RefCell::new(None),
        }
    }

    /// Begin a new frame. Creates a single command encoder for the entire frame.
    /// Call this before render(). If an encoder is already active, it is submitted first.
    pub fn begin_frame(&self) {
        // Submit any pending encoder from a previous frame
        self.submit_pending();
        let encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("frame encoder"),
        });
        *self.encoder.borrow_mut() = Some(encoder);
    }

    /// End the frame. Submits all queued commands.
    pub fn end_frame(&self) {
        self.submit_pending();
    }

    /// Get the current frame's encoder. Panics if begin_frame() was not called.
    pub fn encoder(&self) -> std::cell::Ref<'_, CommandEncoder> {
        std::cell::Ref::map(self.encoder.borrow(), |e| {
            e.as_ref().expect("call begin_frame() before rendering")
        })
    }

    /// Get the current frame's encoder mutably.
    pub fn encoder_mut(&self) -> std::cell::RefMut<'_, CommandEncoder> {
        std::cell::RefMut::map(self.encoder.borrow_mut(), |e| {
            e.as_mut().expect("call begin_frame() before rendering")
        })
    }

    /// Submit and clear the current encoder, if any.
    fn submit_pending(&self) {
        if let Some(encoder) = self.encoder.borrow_mut().take() {
            self.queue.submit(std::iter::once(encoder.finish()));
        }
    }
}
