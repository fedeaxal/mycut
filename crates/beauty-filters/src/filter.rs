use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use wgpu::{BindGroup, BindGroupLayout, RenderPipeline, TextureView};

use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;
use crate::shaders::PASSTHROUGH_WGSL;
use crate::sink::Sink;
use crate::source::Source;

pub struct FilterBase {
    pub output_framebuffer: Option<Framebuffer>,
    pub sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
    pub input_textures: HashMap<u32, (TextureView, u32, u32)>,
    pub width: u32,
    pub height: u32,
}

/// Cached pass-through copy pipeline. Created once, reused every frame.
/// Also caches the bind group to avoid per-frame bind group allocation.
pub struct CopyPass {
    pipeline: RenderPipeline,
    bg_layout: BindGroupLayout,
    dummy_vb: wgpu::Buffer,
    cached_bg: Option<(TextureView, BindGroup)>,
}

impl CopyPass {
    pub fn new(ctx: &GpuContext, label: &str) -> Self {
        Self::new_with_target_format(ctx, label, wgpu::TextureFormat::Rgba8Unorm)
    }

    /// 目标纹理格式可换：BGRA 输出管线的最后一次 copy 写进 Bgra8Unorm，
    /// 回读字节即为 BGRA（shader 仍写 RGBA 语义的颜色值）。
    pub fn new_with_target_format(ctx: &GpuContext, label: &str, format: wgpu::TextureFormat) -> Self {
        let shader = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(PASSTHROUGH_WGSL.into()),
        });
        let bg_layout = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some(label),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2, multisampled: false,
                    }, count: None,
                },
            ],
        });
        let pipeline_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[Some(&bg_layout)],
            immediate_size: 0,
        });
        let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader, entry_point: Some("vs_main"), buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader, entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let dummy_vb = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy vb"),
            size: 4,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });
        Self { pipeline, bg_layout, dummy_vb, cached_bg: None }
    }

    pub fn draw(&mut self, ctx: &GpuContext, src: &TextureView, dst: &TextureView) {
        let bg = if self.cached_bg.as_ref().map(|(v, _)| v == src).unwrap_or(false) {
            self.cached_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.bg_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(src) },
                ],
                label: Some("copy bg"),
            });
            self.cached_bg = Some((src.clone(), bg.clone()));
            bg
        };
        {
            let mut encoder = ctx.encoder_mut();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("copy pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: dst, resolve_target: None, depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None, timestamp_writes: None, occlusion_query_set: None, multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_vertex_buffer(0, self.dummy_vb.slice(..));
            pass.draw(0..4, 0..1);
        }
    }
}

impl FilterBase {
    pub fn new() -> Self {
        Self {
            output_framebuffer: None,
            sinks: Vec::new(),
            input_textures: HashMap::new(),
            width: 0,
            height: 0,
        }
    }

    pub fn ensure_framebuffer(&mut self, ctx: &GpuContext, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let needs_new = match &self.output_framebuffer {
            Some(fb) => fb.width != width || fb.height != height,
            None => true,
        };
        if needs_new {
            self.output_framebuffer = Some(Framebuffer::new(ctx, width, height));
        }
    }

    pub fn create_bind_group(
        ctx: &GpuContext,
        layout: &BindGroupLayout,
        uniform_buffer: Option<&wgpu::Buffer>,
        _uniform_size: u64,
        texture_views: &[&TextureView],
    ) -> BindGroup {
        let mut entries: Vec<wgpu::BindGroupEntry> = Vec::new();
        let mut binding = 0u32;

        if let Some(buf) = uniform_buffer {
            entries.push(wgpu::BindGroupEntry {
                binding,
                resource: buf.as_entire_binding(),
            });
            binding += 1;
        }

        entries.push(wgpu::BindGroupEntry {
            binding,
            resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler),
        });
        binding += 1;

        for tv in texture_views {
            entries.push(wgpu::BindGroupEntry {
                binding,
                resource: wgpu::BindingResource::TextureView(tv),
            });
            binding += 1;
        }

        ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("filter bind group"),
            layout,
            entries: &entries,
        })
    }

    /// Draw a fullscreen quad using the shared frame encoder.
    /// Call ctx.begin_frame() before using this.
    pub fn draw_quad_pass(
        ctx: &GpuContext,
        pipeline: &RenderPipeline,
        bind_group: &BindGroup,
        target_view: &TextureView,
        vertex_buffer: &wgpu::Buffer,
    ) {
        let mut encoder = ctx.encoder_mut();
        let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("filter pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
        render_pass.draw(0..4, 0..1);
    }

    pub fn create_uniform_buffer(ctx: &GpuContext, size: u64, label: &str) -> wgpu::Buffer {
        ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    pub fn create_bind_group_layout(
        ctx: &GpuContext,
        has_uniform: bool,
        num_textures: usize,
    ) -> BindGroupLayout {
        let mut entries = Vec::new();

        if has_uniform {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            });
        }

        let sampler_binding = if has_uniform { 1 } else { 0 };
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: sampler_binding,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        });

        let tex_start = sampler_binding + 1;
        for i in 0..num_textures {
            entries.push(wgpu::BindGroupLayoutEntry {
                binding: tex_start + i as u32,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            });
        }

        ctx.device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("filter bind group layout"),
                entries: &entries,
            })
    }
}

impl Source for FilterBase {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        self.output_framebuffer.as_ref().expect("Framebuffer not initialized")
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        self.sinks.push((sink, texture_slot));
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        &self.sinks
    }

    fn render(&mut self, _ctx: &GpuContext) {
        // Base implementation does nothing; subclasses override
    }
}
