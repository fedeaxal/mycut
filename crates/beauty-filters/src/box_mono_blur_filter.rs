use wgpu::TextureView;

use crate::context::GpuContext;
use crate::filter::FilterBase;
use crate::framebuffer::Framebuffer;
use crate::pipeline_cache::PipelineCache;
use crate::sink::Sink;
use crate::source::Source;

use std::cell::RefCell;
use std::rc::Rc;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurUniforms {
    texel_width_offset: f32,
    texel_height_offset: f32,
    _pad: [f32; 2],
}

pub enum BlurDirection {
    Horizontal,
    Vertical,
}

/// Generate a box blur WGSL shader for the given radius.
/// Matches C++ GPUPixel BoxMonoBlurFilter:
///   num_optimized_offsets = min(radius / 2 + (radius % 2), 7)
///   offsets = (i * 2 + 1.5) for i in 0..num_offsets
///   box_weight = 1.0 / (radius * 2 + 1)
fn generate_blur_wgsl(radius: i32) -> String {
    let num_offsets = (radius / 2 + radius % 2).min(7) as usize;
    let box_weight_denom = (radius * 2 + 1) as f32;

    let mut s = String::new();

    // Uniforms
    s.push_str("struct Uniforms { texel_width_offset: f32, texel_height_offset: f32, _pad: vec2f };\n");

    // Vertex output
    s.push_str("struct VertexOutput { @builtin(position) pos: vec4f");
    for i in 0..=num_offsets * 2 {
        s.push_str(&format!(", @location({}) blurCoordinates_{}: vec2f", i, i));
    }
    s.push_str(" };\n");

    // Bindings
    s.push_str("@group(0) @binding(0) var<uniform> params: Uniforms;\n");
    s.push_str("@group(0) @binding(1) var samp: sampler;\n");
    s.push_str("@group(0) @binding(2) var tex: texture_2d<f32>;\n\n");

    // Vertex shader
    s.push_str("@vertex\nfn vs_main(@builtin(vertex_index) vid: u32) -> VertexOutput {\n");
    s.push_str("    var positions = array<vec2f, 4>(vec2f(-1,-1),vec2f(1,-1),vec2f(-1,1),vec2f(1,1));\n");
    s.push_str("    var uvs = array<vec2f, 4>(vec2f(0,1),vec2f(1,1),vec2f(0,0),vec2f(1,0));\n");
    s.push_str("    let uv = uvs[vid];\n");
    s.push_str("    let singleStepOffset = vec2f(params.texel_width_offset, params.texel_height_offset);\n");
    s.push_str("    var out: VertexOutput;\n");
    s.push_str("    out.pos = vec4f(positions[vid], 0.0, 1.0);\n");
    s.push_str("    out.blurCoordinates_0 = uv;\n");

    let mut loc = 1;
    for i in 0..num_offsets {
        let offset = (i as f32) * 2.0 + 1.5;
        s.push_str(&format!(
            "    out.blurCoordinates_{} = uv + singleStepOffset * {};\n",
            loc, offset
        ));
        loc += 1;
        s.push_str(&format!(
            "    out.blurCoordinates_{} = uv - singleStepOffset * {};\n",
            loc, offset
        ));
        loc += 1;
    }
    s.push_str("    return out;\n}\n\n");

    // Fragment shader
    s.push_str("@fragment\nfn fs_main(in: VertexOutput) -> @location(0) vec4f {\n");
    s.push_str(&format!(
        "    let boxWeight = 1.0 / {};\n",
        box_weight_denom
    ));
    s.push_str("    var sum = textureSample(tex, samp, in.blurCoordinates_0) * boxWeight;\n");
    for i in 1..loc {
        s.push_str(&format!(
            "    sum += textureSample(tex, samp, in.blurCoordinates_{}) * (boxWeight * 2.0);\n",
            i
        ));
    }
    s.push_str("    return sum;\n}\n");

    s
}

/// Compute scaled radius based on face width (pixels).
/// At baseline face width ~200px (640x480 video), base_radius is used as-is.
/// Radius scales linearly with face width so the blur covers the same
/// proportion of the face regardless of image resolution.
pub fn radius_for_face(base_radius: i32, face_width: f32) -> i32 {
    const BASE_FACE_WIDTH: f32 = 200.0;
    let r = (base_radius as f32 * face_width / BASE_FACE_WIDTH).round() as i32;
    r.max(1).min(14)
}

pub struct BoxMonoBlurFilter {
    base: FilterBase,
    pipeline_cache: PipelineCache,
    uniform_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    dummy_vb: wgpu::Buffer,
    direction: BlurDirection,
    _base_radius: i32,
    current_radius: i32,
    /// 采样间隔(像素数):C++ GaussianBlurMonoFilter 的 texel spacing。
    /// 默认 1 = 相邻像素;BeautyFaceFilter 磨皮 blur 设为 4(与 C++ Init 的
    /// SetTexelSpacingMultiplier(4) 一致),采样偏移 = spacing / width。
    texel_spacing: f32,
    shader: String,
    cached_bg: Option<(TextureView, wgpu::BindGroup)>,
}

impl BoxMonoBlurFilter {
    pub fn new(ctx: &GpuContext, direction: BlurDirection, radius: i32) -> Self {
        let uniform_buffer = FilterBase::create_uniform_buffer(ctx, 16, "blur uniforms");
        let bind_group_layout = FilterBase::create_bind_group_layout(ctx, true, 1);
        let dummy_vb = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy vb"),
            size: 4,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });

        let shader = generate_blur_wgsl(radius);

        Self {
            base: FilterBase::new(),
            pipeline_cache: PipelineCache::new(),
            uniform_buffer,
            bind_group_layout,
            dummy_vb,
            direction,
            _base_radius: radius,
            current_radius: radius,
            texel_spacing: 1.0,
            shader,
            cached_bg: None,
        }
    }

    /// Update radius (regenerates shader if changed).
    pub fn set_radius(&mut self, radius: i32) {
        if radius != self.current_radius {
            self.current_radius = radius;
            self.shader = generate_blur_wgsl(radius);
        }
    }

    /// Set sampling interval in pixels (C++ SetTexelSpacingMultiplier).
    /// Offset = spacing / texture size, so 1 = adjacent texels, 4 = every 4th.
    pub fn set_texel_spacing_multiplier(&mut self, spacing: f32) {
        self.texel_spacing = spacing;
    }

    fn pipeline_key(&self) -> String {
        let dir = match self.direction {
            BlurDirection::Horizontal => "h",
            BlurDirection::Vertical => "v",
        };
        format!("box_blur_{}_r{}", dir, self.current_radius)
    }
}

impl Source for BoxMonoBlurFilter {
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

        let (tw, th) = match self.direction {
            BlurDirection::Horizontal => (self.texel_spacing / w as f32, 0.0),
            BlurDirection::Vertical => (0.0, self.texel_spacing / h as f32),
        };

        let uniforms = BlurUniforms {
            texel_width_offset: tw,
            texel_height_offset: th,
            _pad: [0.0; 2],
        };
        ctx.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&[uniforms]));

        let views: Vec<&TextureView> = [&view].to_vec();
        let bind_group = if self.cached_bg.as_ref().map(|(v, _)| v == &view).unwrap_or(false) {
            self.cached_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = FilterBase::create_bind_group(
                ctx, &self.bind_group_layout,
                Some(&self.uniform_buffer), 16, &views,
            );
            self.cached_bg = Some((view.clone(), bg.clone()));
            bg
        };

        let key = self.pipeline_key();
        let shader_str: &str = &self.shader;
        let layout = &self.bind_group_layout;
        let pipeline = self.pipeline_cache.get_or_create(
            &ctx.device, &key, shader_str,
            layout,
            wgpu::VertexBufferLayout {
                array_stride: 0, step_mode: wgpu::VertexStepMode::Vertex, attributes: &[],
            },
        );

        let fb = self.base.get_output_framebuffer();
        FilterBase::draw_quad_pass(ctx, pipeline, &bind_group, &fb.view, &self.dummy_vb);
        self.propagate_to_sinks(ctx);
    }
}

impl Sink for BoxMonoBlurFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, _ctx: &GpuContext) {
        self.base.input_textures.insert(slot, (fb.view.clone(), fb.width, fb.height));
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
