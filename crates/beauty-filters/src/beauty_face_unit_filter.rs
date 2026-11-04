use wgpu::TextureView;

use crate::context::GpuContext;
use crate::filter::FilterBase;
use crate::filter_group::FilterNode;
use crate::framebuffer::Framebuffer;
use crate::pipeline_cache::PipelineCache;
use crate::shaders::BEAUTY_FACE_UNIT_WGSL;
use crate::sink::Sink;
use crate::source::Source;
use crate::texture_loader::load_png_from_bytes;

use std::cell::RefCell;
use std::rc::Rc;

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BeautyUniforms {
    width_offset: f32,
    height_offset: f32,
    sharpen: f32,
    blur_alpha: f32,
    whiten: f32,
    _pad: [f32; 3],
}

pub struct BeautyFaceUnitFilter {
    base: FilterBase,
    pipeline_cache: PipelineCache,
    uniform_buffer: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,
    dummy_vb: wgpu::Buffer,
    lookup_gray_view: TextureView,
    lookup_origin_view: TextureView,
    lookup_skin_view: TextureView,
    lookup_custom_view: TextureView,
    sharpen: f32,
    blur_alpha: f32,
    whiten: f32,
    /// Cached bind group keyed on (slot0 view, slot1 view, slot2 view)
    cached_bg: Option<((TextureView, TextureView, TextureView), wgpu::BindGroup)>,
}

impl BeautyFaceUnitFilter {
    pub fn new(ctx: &GpuContext) -> Self {
        let uniform_buffer = FilterBase::create_uniform_buffer(ctx, 32, "beauty unit uniforms");
        let bind_group_layout = FilterBase::create_bind_group_layout(ctx, true, 7);
        let dummy_vb = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy vb"),
            size: 4,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });

        // Load lookup textures — embedded at compile time for zero filesystem I/O
        let (tex, _, _) = load_png_from_bytes(ctx, include_bytes!("res/lookup_gray.png"));
        let lookup_gray_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let (tex, _, _) = load_png_from_bytes(ctx, include_bytes!("res/lookup_origin.png"));
        let lookup_origin_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let (tex, _, _) = load_png_from_bytes(ctx, include_bytes!("res/lookup_skin.png"));
        let lookup_skin_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        let (tex, _, _) = load_png_from_bytes(ctx, include_bytes!("res/lookup_light.png"));
        let lookup_custom_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

        Self {
            base: FilterBase::new(),
            pipeline_cache: PipelineCache::new(),
            uniform_buffer,
            bind_group_layout,
            dummy_vb,
            lookup_gray_view,
            lookup_origin_view,
            lookup_skin_view,
            lookup_custom_view,
            sharpen: 0.0,
            blur_alpha: 0.0,
            whiten: 0.0,
            cached_bg: None,
        }
    }

    pub fn set_sharpen(&mut self, v: f32) { self.sharpen = v; }
    pub fn set_blur_alpha(&mut self, v: f32) { self.blur_alpha = v; }
    pub fn set_whiten(&mut self, v: f32) { self.whiten = v; }

    /// Debug: get a clone of the input texture view at the given slot
    pub fn get_input_texture_view(&self, slot: u32) -> Option<wgpu::TextureView> {
        self.base.input_textures.get(&slot).map(|(v, _, _)| v.clone())
    }
}

impl FilterNode for BeautyFaceUnitFilter {}

impl Source for BeautyFaceUnitFilter {
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
        let (view0, w, h) = self.base.input_textures.get(&0)
            .map(|(v, w, h)| (v.clone(), *w, *h))
            .expect("No input texture 0");
        let (view1, _, _) = self.base.input_textures.get(&1)
            .map(|(v, w, h)| (v.clone(), *w, *h))
            .expect("No input texture 1");
        let (view2, _, _) = self.base.input_textures.get(&2)
            .map(|(v, w, h)| (v.clone(), *w, *h))
            .expect("No input texture 2");

        self.base.ensure_framebuffer(ctx, w, h);

        let uniforms = BeautyUniforms {
            width_offset: 1.0 / w as f32,
            height_offset: 1.0 / h as f32,
            sharpen: self.sharpen,
            blur_alpha: self.blur_alpha,
            whiten: self.whiten,
            _pad: [0.0; 3],
        };
        ctx.queue.write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&[uniforms]));

        let views: Vec<&TextureView> = [
            &view0, &view1, &view2,
            &self.lookup_gray_view, &self.lookup_origin_view,
            &self.lookup_skin_view, &self.lookup_custom_view,
        ].to_vec();

        let key = (view0.clone(), view1.clone(), view2.clone());
        let bind_group = if self.cached_bg.as_ref().map(|(k, _)| k == &key).unwrap_or(false) {
            self.cached_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = FilterBase::create_bind_group(
                ctx, &self.bind_group_layout,
                Some(&self.uniform_buffer), 32, &views,
            );
            self.cached_bg = Some((key, bg.clone()));
            bg
        };

        let pipeline = self.pipeline_cache.get_or_create(
            &ctx.device, "beauty_face_unit", BEAUTY_FACE_UNIT_WGSL,
            &self.bind_group_layout,
            wgpu::VertexBufferLayout {
                array_stride: 0, step_mode: wgpu::VertexStepMode::Vertex, attributes: &[],
            },
        );

        let fb = self.base.get_output_framebuffer();
        FilterBase::draw_quad_pass(ctx, pipeline, &bind_group, &fb.view, &self.dummy_vb);
        self.propagate_to_sinks(ctx);
    }
}

impl Sink for BeautyFaceUnitFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, _ctx: &GpuContext) {
        self.base.input_textures.insert(slot, (fb.view.clone(), fb.width, fb.height));
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
