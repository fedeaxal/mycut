use wgpu::TextureView;

use crate::context::GpuContext;
use crate::filter::FilterBase;
use crate::framebuffer::Framebuffer;
use crate::pipeline_cache::PipelineCache;
use crate::shaders::BRIGHTNESS_WGSL;
use crate::sink::Sink;
use crate::source::Source;

use std::cell::RefCell;
use std::rc::Rc;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BrightnessUniforms {
    brightness: f32,
    _pad: [f32; 3],
}

pub struct BrightnessFilter {
    base: FilterBase,
    pipeline_cache: PipelineCache,
    uniform_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    dummy_vb: wgpu::Buffer,
    brightness: f32,
    cached_bg: Option<(TextureView, wgpu::BindGroup)>,
}

impl BrightnessFilter {
    pub fn new(ctx: &GpuContext) -> Self {
        let uniform_buffer = FilterBase::create_uniform_buffer(ctx, 16, "brightness uniforms");
        let bind_group_layout = FilterBase::create_bind_group_layout(ctx, true, 1);
        let dummy_vb = create_dummy_vb(ctx);

        Self {
            base: FilterBase::new(),
            pipeline_cache: PipelineCache::new(),
            uniform_buffer,
            bind_group_layout,
            dummy_vb,
            brightness: 0.0,
            cached_bg: None,
        }
    }

    pub fn set_brightness(&mut self, brightness: f32) {
        self.brightness = brightness;
    }

    fn update_uniforms(&self, ctx: &GpuContext) {
        let uniforms = BrightnessUniforms {
            brightness: self.brightness,
            _pad: [0.0; 3],
        };
        ctx.queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::cast_slice(&[uniforms]),
        );
    }
}

impl Source for BrightnessFilter {
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
            .expect("No input texture at slot 0");
        self.base.ensure_framebuffer(ctx, w, h);
        self.update_uniforms(ctx);

        let views: Vec<&TextureView> = [&view as &TextureView].to_vec();
        let bind_group = if self.cached_bg.as_ref().map(|(v, _)| v == &view).unwrap_or(false) {
            self.cached_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = FilterBase::create_bind_group(
                ctx,
                &self.bind_group_layout,
                Some(&self.uniform_buffer),
                16,
                &views,
            );
            self.cached_bg = Some((view.clone(), bg.clone()));
            bg
        };

        let pipeline = self.pipeline_cache.get_or_create(
            &ctx.device,
            "brightness",
            BRIGHTNESS_WGSL,
            &self.bind_group_layout,
            wgpu::VertexBufferLayout {
                array_stride: 0,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &[],
            },
        );

        let fb = self.base.get_output_framebuffer();
        FilterBase::draw_quad_pass(ctx, pipeline, &bind_group, &fb.view, &self.dummy_vb);

        self.propagate_and_render_sinks(ctx);
    }
}

fn create_dummy_vb(ctx: &GpuContext) -> wgpu::Buffer {
    ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dummy vb"),
        size: 4,
        usage: wgpu::BufferUsages::VERTEX,
        mapped_at_creation: false,
    })
}

// Need to implement Sink for BrightnessFilter
impl Sink for BrightnessFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, _ctx: &GpuContext) {
        self.base.input_textures.insert(slot, (fb.view.clone(), fb.width, fb.height));
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
