use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;
use crate::texture_loader::load_image_to_texture;

use std::cell::RefCell;
use std::rc::Rc;

pub struct SourceImage {
    framebuffer: Framebuffer,
    sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
}

impl SourceImage {
    pub fn new(ctx: &GpuContext, path: &str) -> Self {
        let img = image::open(path).unwrap_or_else(|e| panic!("Failed to open {}: {}", path, e));
        let (texture, w, h) = load_image_to_texture(ctx, &img);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            framebuffer: Framebuffer {
                texture,
                view,
                width: w,
                height: h,
            },
            sinks: Vec::new(),
        }
    }

    pub fn from_image(ctx: &GpuContext, img: &image::DynamicImage) -> Self {
        let (texture, w, h) = load_image_to_texture(ctx, img);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            framebuffer: Framebuffer {
                texture,
                view,
                width: w,
                height: h,
            },
            sinks: Vec::new(),
        }
    }

    pub fn width(&self) -> u32 {
        self.framebuffer.width
    }

    pub fn height(&self) -> u32 {
        self.framebuffer.height
    }
}

impl Source for SourceImage {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        self.sinks.push((sink, texture_slot));
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        &self.sinks
    }

    fn render(&mut self, ctx: &GpuContext) {
        self.propagate_to_sinks(ctx);
        let sinks: Vec<_> = self.get_sinks().to_vec();
        for (sink, _) in &sinks {
            sink.borrow_mut().render(ctx);
        }
    }
}
