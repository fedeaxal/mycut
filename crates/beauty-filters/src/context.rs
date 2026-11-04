use wgpu::{CommandEncoder, Device, Queue, Sampler};
use std::cell::RefCell;

pub struct GpuContext {
    pub device: Device,
    pub queue: Queue,
    pub linear_sampler: Sampler,
    /// Per-frame encoder. Created in begin_frame(), submitted in end_frame().
    encoder: RefCell<Option<CommandEncoder>>,
}

impl GpuContext {
    pub fn new() -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
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
