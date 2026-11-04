use crate::context::GpuContext;
use crate::face_makeup_filter::FaceMakeupFilter;
use crate::filter_group::FilterNode;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;
use crate::texture_loader::load_png_from_bytes;

use std::cell::RefCell;
use std::rc::Rc;

pub struct BlusherFilter {
    inner: FaceMakeupFilter,
}

impl BlusherFilter {
    pub fn new(ctx: &GpuContext) -> Self {
        let mut inner = FaceMakeupFilter::new(ctx);

        // Embedded at compile time — zero filesystem I/O
        let (tex, _, _) = load_png_from_bytes(ctx, include_bytes!("res/blusher.png"));
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        inner.set_makeup_texture(view);
        inner.set_texture_bounds(395.0, 520.0, 489.0, 209.0);
        inner.set_blend_mode(15);

        Self { inner }
    }

    pub fn set_face_landmarks(&mut self, landmarks: Vec<f32>) {
        self.inner.set_face_landmarks(landmarks);
    }

    pub fn set_intensity(&mut self, level: f32) {
        self.inner.set_blend_level(level);
    }
}

impl FilterNode for BlusherFilter {}

impl Source for BlusherFilter {
    fn get_output_framebuffer(&self) -> &Framebuffer { self.inner.get_output_framebuffer() }
    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, slot: u32) { self.inner.add_sink(sink, slot); }
    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] { self.inner.get_sinks() }
    fn render(&mut self, ctx: &GpuContext) { Source::render(&mut self.inner, ctx); }
}

impl Sink for BlusherFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, ctx: &GpuContext) {
        self.inner.set_input_framebuffer(fb, slot, ctx);
    }
    fn render(&mut self, ctx: &GpuContext) { Sink::render(&mut self.inner, ctx); }
}
