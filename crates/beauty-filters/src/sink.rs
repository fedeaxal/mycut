use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;

pub trait Sink {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, ctx: &GpuContext);
    fn render(&mut self, ctx: &GpuContext);
}
