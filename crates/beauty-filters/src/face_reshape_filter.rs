
use crate::context::GpuContext;
use crate::filter::FilterBase;
use crate::filter_group::FilterNode;
use crate::framebuffer::Framebuffer;
use crate::pipeline_cache::PipelineCache;
use crate::shaders::FACE_RESHAPE_WGSL;
use crate::sink::Sink;
use crate::source::Source;

use std::cell::RefCell;
use std::rc::Rc;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ReshapeUniforms {
    aspect_ratio: f32,
    thin_face_delta: f32,
    big_eye_delta: f32,
    has_face: u32,
}

pub struct FaceReshapeFilter {
    base: FilterBase,
    pipeline_cache: PipelineCache,
    uniform_buffer: wgpu::Buffer,
    storage_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    dummy_vb: wgpu::Buffer,
    cached_bg: Option<(wgpu::TextureView, wgpu::BindGroup)>,
    thin_face_delta: f32,
    big_eye_delta: f32,
    face_landmarks: Vec<f32>,
    has_face: bool,
}

impl FaceReshapeFilter {
    pub fn new(ctx: &GpuContext) -> Self {
        let uniform_buffer = FilterBase::create_uniform_buffer(ctx, 16, "reshape uniforms");

        // Storage buffer for 106 face points × 2 floats = 212 floats
        let storage_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("face points storage"),
            size: 212 * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("reshape bg layout"),
            entries: &[
                // Uniforms
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // Sampler
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                // Input texture
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // Storage buffer
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let dummy_vb = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy vb"),
            size: 4,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });

        Self {
            base: FilterBase::new(),
            pipeline_cache: PipelineCache::new(),
            uniform_buffer,
            storage_buffer,
            bind_group_layout,
            dummy_vb,
            cached_bg: None,
            thin_face_delta: 0.0,
            big_eye_delta: 0.0,
            face_landmarks: Vec::new(),
            has_face: false,
        }
    }

    pub fn set_face_landmarks(&mut self, landmarks: Vec<f32>) {
        if landmarks.is_empty() {
            self.has_face = false;
        } else {
            self.face_landmarks = landmarks;
            self.has_face = true;
        }
    }

    pub fn set_face_slim_level(&mut self, level: f32) {
        self.thin_face_delta = level;
    }

    pub fn set_eye_zoom_level(&mut self, level: f32) {
        self.big_eye_delta = level;
    }
}

impl FilterNode for FaceReshapeFilter {}

impl Source for FaceReshapeFilter {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        self.base.get_output_framebuffer()
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        self.base.add_sink(sink, texture_slot);
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        self.base.get_sinks()
    }

    fn render(&mut self, ctx: &GpuContext) {
        let (view, w, h) = self.base.input_textures.get(&0)
            .map(|(v, w, h)| (v.clone(), *w, *h))
            .expect("No input texture");
        self.base.ensure_framebuffer(ctx, w, h);

        // Upload face landmarks to storage buffer
        let mut data = [0.0f32; 212];
        if self.has_face && self.face_landmarks.len() >= 212 {
            data.copy_from_slice(&self.face_landmarks[..212]);
        }
        ctx.queue.write_buffer(&self.storage_buffer, 0, bytemuck::cast_slice(&data));

        let uniforms = ReshapeUniforms {
            aspect_ratio: w as f32 / h as f32,
            thin_face_delta: self.thin_face_delta,
            big_eye_delta: self.big_eye_delta,
            has_face: if self.has_face { 1 } else { 0 },
        };
        ctx.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&[uniforms]));

        let bind_group = if self.cached_bg.as_ref().map(|(v, _)| v == &view).unwrap_or(false) {
            self.cached_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.uniform_buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&view) },
                    wgpu::BindGroupEntry { binding: 3, resource: self.storage_buffer.as_entire_binding() },
                ],
                label: Some("reshape bg"),
            });
            self.cached_bg = Some((view, bg.clone()));
            bg
        };

        let pipeline = self.pipeline_cache.get_or_create(
            &ctx.device, "face_reshape", FACE_RESHAPE_WGSL,
            &self.bind_group_layout,
            wgpu::VertexBufferLayout {
                array_stride: 0, step_mode: wgpu::VertexStepMode::Vertex, attributes: &[],
            },
        );

        let fb = self.base.get_output_framebuffer();
        FilterBase::draw_quad_pass(ctx, pipeline, &bind_group, &fb.view, &self.dummy_vb);
        self.propagate_and_render_sinks(ctx);
    }
}

impl Sink for FaceReshapeFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, _ctx: &GpuContext) {
        self.base.input_textures.insert(slot, (fb.view.clone(), fb.width, fb.height));
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
