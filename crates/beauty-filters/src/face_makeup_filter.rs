use wgpu::TextureView;
use wgpu::util::DeviceExt;

use crate::context::GpuContext;
use crate::filter::FilterBase;
use crate::filter_group::FilterNode;
use crate::framebuffer::Framebuffer;
use crate::pipeline_cache::PipelineCache;
use crate::shaders::{FACE_MAKEUP_WGSL, PASSTHROUGH_WGSL};
use crate::sink::Sink;
use crate::source::Source;
use crate::vertex::{FACE_MESH_INDICES, FACE_TEXTURE_COORDINATES};

use std::cell::RefCell;
use std::rc::Rc;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MakeupUniforms {
    intensity: f32,
    blend_mode: i32,
    _pad: [f32; 2],
}

pub struct FaceMakeupFilter {
    base: FilterBase,
    pipeline_cache: PipelineCache,
    uniform_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    passthrough_pipeline: Option<wgpu::RenderPipeline>,
    passthrough_bg_layout: Option<wgpu::BindGroupLayout>,
    /// Cached passthrough bind group, keyed on input TextureView
    cached_passthrough_bg: Option<(TextureView, wgpu::BindGroup)>,
    /// Cached makeup bind group, keyed on input TextureView
    cached_makeup_bg: Option<(TextureView, wgpu::BindGroup)>,
    /// 人脸网格顶点 buffer:固定分配(111 顶点 × 4 f32),landmarks 变化时 write_buffer 覆写
    /// (替代每帧 create_buffer_init 新建,省一次 GPU 资源分配)。
    vertex_buffer: wgpu::Buffer,
    landmarks_dirty: bool,
    index_buffer: wgpu::Buffer,
    blend_level: f32,
    blend_mode: i32,
    has_face: bool,
    face_landmarks: Vec<f32>,
    /// Texture bounds: {x, y, width, height}
    texture_bounds: (f32, f32, f32, f32),
    makeup_view: Option<TextureView>,
}

impl FaceMakeupFilter {
    pub fn new(ctx: &GpuContext) -> Self {
        let uniform_buffer = FilterBase::create_uniform_buffer(ctx, 16, "makeup uniforms");

        let bind_group_layout = FilterBase::create_bind_group_layout(ctx, true, 2);

        let index_buffer = ctx.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("face mesh index buffer"),
            contents: bytemuck::cast_slice(FACE_MESH_INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        // 111 顶点(106 关键点 + 5 中点填充)× 4 f32(pos.xy + uv.xy),会话内复用。
        let vertex_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("face mesh vertex buffer"),
            size: (111 * 4 * 4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            base: FilterBase::new(),
            pipeline_cache: PipelineCache::new(),
            uniform_buffer,
            bind_group_layout,
            passthrough_pipeline: None,
            passthrough_bg_layout: None,
            cached_passthrough_bg: None,
            cached_makeup_bg: None,
            vertex_buffer,
            landmarks_dirty: true,
            index_buffer,
            blend_level: 0.0,
            blend_mode: 15, // Multiply
            has_face: false,
            face_landmarks: Vec::new(),
            texture_bounds: (0.0, 0.0, 1280.0, 1280.0),
            makeup_view: None,
        }
    }

    pub fn set_makeup_texture(&mut self, view: TextureView) {
        self.makeup_view = Some(view);
    }

    pub fn set_texture_bounds(&mut self, x: f32, y: f32, width: f32, height: f32) {
        self.texture_bounds = (x, y, width, height);
    }

    pub fn set_face_landmarks(&mut self, landmarks: Vec<f32>) {
        if landmarks.is_empty() {
            self.has_face = false;
        } else {
            // Convert from [0,1] image coords (Y-down) to [-1,1] NDC (Y-up).
            // X: [0,1] → [-1,1], Y: [0,1] → [1,-1] (flipped)
            self.face_landmarks = landmarks.iter().enumerate().map(|(i, v)| {
                if i % 2 == 1 { 1.0 - 2.0 * v } else { 2.0 * v - 1.0 }
            }).collect();
            self.has_face = true;
        }
        self.landmarks_dirty = true;
    }

    pub fn set_blend_level(&mut self, level: f32) {
        self.blend_level = level;
    }

    pub fn set_blend_mode(&mut self, mode: i32) {
        self.blend_mode = mode;
    }
}

impl FilterNode for FaceMakeupFilter {}

impl Source for FaceMakeupFilter {
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
        let (input_view, w, h) = self.base.input_textures.get(&0)
            .map(|(v, w, h)| (v.clone(), *w, *h))
            .expect("No input texture");
        self.base.ensure_framebuffer(ctx, w, h);

        let makeup_view = self.makeup_view.as_ref().expect("No makeup texture set");
        let fb = self.base.get_output_framebuffer();

        // === Pass 1: Passthrough (copy original) ===
        if self.passthrough_pipeline.is_none() {
            let module = ctx.device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("makeup passthrough"),
                source: wgpu::ShaderSource::Wgsl(PASSTHROUGH_WGSL.into()),
            });
            let bg_layout = ctx.device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("passthrough bg layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            });
            let pipeline_layout = ctx.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("passthrough layout"),
                bind_group_layouts: &[Some(&bg_layout)],
            immediate_size: 0,
            });
            let pipeline = ctx.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("passthrough pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_main"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs_main"),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleStrip,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
            self.passthrough_pipeline = Some(pipeline);
            self.passthrough_bg_layout = Some(bg_layout);
        }

        let input_view_id = input_view.clone();

        // Use cached passthrough bind group if input view hasn't changed
        let passthrough_bg = if self.cached_passthrough_bg.as_ref().map(|(v, _)| v == &input_view_id).unwrap_or(false) {
            self.cached_passthrough_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: self.passthrough_bg_layout.as_ref().unwrap(),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Sampler(&ctx.linear_sampler) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&input_view) },
                ],
                label: Some("passthrough bg"),
            });
            self.cached_passthrough_bg = Some((input_view_id.clone(), bg.clone()));
            bg
        };

        // Uniforms
        let uniforms = MakeupUniforms {
            intensity: self.blend_level,
            blend_mode: self.blend_mode,
            _pad: [0.0; 2],
        };
        ctx.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&[uniforms]));

        let views: Vec<&TextureView> = [&input_view, makeup_view].to_vec();
        // Use cached makeup bind group if input view hasn't changed
        let makeup_bg = if self.cached_makeup_bg.as_ref().map(|(v, _)| v == &input_view_id).unwrap_or(false) {
            self.cached_makeup_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = FilterBase::create_bind_group(
                ctx, &self.bind_group_layout,
                Some(&self.uniform_buffer), 16, &views,
            );
            self.cached_makeup_bg = Some((input_view_id, bg.clone()));
            bg
        };

        // === Combined render pass ===
        // Pass 1: Draw passthrough (copy original to output)
        {
            let mut encoder = ctx.encoder_mut();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("makeup passthrough pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &fb.view,
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
            pass.set_pipeline(self.passthrough_pipeline.as_ref().unwrap());
            pass.set_bind_group(0, &passthrough_bg, &[]);
            pass.draw(0..4, 0..1);
        }

        // Pass 2: Draw face mesh overlay (if face detected)
        if self.has_face && self.face_landmarks.len() >= 212 {
            // Prepare vertex data: positions from landmarks, UVs from face texture coords
            // 111 vertices total: 106 face landmarks + 5 midpoint fill vertices (106-110)
            let (bx, by, bw, bh) = self.texture_bounds;
            let mut vertex_data: Vec<f32> = Vec::with_capacity(111 * 4);
            for i in 0..111 {
                let (px, py) = if i < 106 {
                    (self.face_landmarks[i * 2], self.face_landmarks[i * 2 + 1])
                } else {
                    // Midpoint vertices for filling mesh gaps
                    let (a, b) = match i {
                        106 => (98, 102), // mouth inner center
                        107 => (35, 66),  // left brow fill
                        108 => (40, 70),  // right brow fill
                        109 => (5, 80),   // left cheek fill
                        110 => (60, 81),  // right cheek fill
                        _ => (0, 0),
                    };
                    let px = (self.face_landmarks[a * 2] + self.face_landmarks[b * 2]) / 2.0;
                    let py = (self.face_landmarks[a * 2 + 1] + self.face_landmarks[b * 2 + 1]) / 2.0;
                    (px, py)
                };
                // Texture coordinate: remap from face texture coordinates to makeup texture space
                let fc_x = FACE_TEXTURE_COORDINATES[i * 2];
                let fc_y = FACE_TEXTURE_COORDINATES[i * 2 + 1];
                let tu = (fc_x * 1280.0 - bx) / bw;
                let tv = (fc_y * 1280.0 - by) / bh;
                vertex_data.push(px); // pos x
                vertex_data.push(py); // pos y
                vertex_data.push(tu); // tex u
                vertex_data.push(tv); // tex v
            }

            // 覆写固定 vertex buffer(landmarks 变化时才写;复用避免每帧 GPU 资源分配)
            if self.landmarks_dirty {
                ctx.queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(&vertex_data));
                self.landmarks_dirty = false;
            }
            let vertex_buffer = &self.vertex_buffer;

            // Create indexed pipeline using vs_mesh entry point that reads from vertex buffer
            let makeup_pipeline = self.pipeline_cache.get_or_create_indexed(
                &ctx.device, "face_makeup_indexed", FACE_MAKEUP_WGSL,
                &self.bind_group_layout,
                wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![
                        0 => Float32x2, // pos
                        1 => Float32x2, // uv
                    ],
                },
                "vs_mesh",
            );

            {
                let mut encoder = ctx.encoder_mut();
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("makeup mesh pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &fb.view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass.set_pipeline(makeup_pipeline);
                pass.set_bind_group(0, &makeup_bg, &[]);
                pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..FACE_MESH_INDICES.len() as u32, 0, 0..1);
            }
        }

        self.propagate_and_render_sinks(ctx);
    }
}

impl Sink for FaceMakeupFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, _ctx: &GpuContext) {
        self.base.input_textures.insert(slot, (fb.view.clone(), fb.width, fb.height));
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
