use std::cell::RefCell;
use std::rc::Rc;

use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;

pub trait Source {
    fn get_output_framebuffer(&self) -> &Framebuffer;
    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32);
    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)];
    fn render(&mut self, ctx: &GpuContext);

    /// Propagate output framebuffer to all sinks (set their input textures).
    /// Does NOT call render on sinks - the caller manages rendering order.
    fn propagate_to_sinks(&self, ctx: &GpuContext) {
        let fb = self.get_output_framebuffer();
        for (sink, slot) in self.get_sinks() {
            sink.borrow_mut().set_input_framebuffer(fb, *slot, ctx);
        }
    }

    /// Propagate output to sinks AND recursively render them.
    /// Matches C++ GPUPixel's Source::DoUpdateSinks() behavior:
    /// for each sink: set input framebuffer, then call render().
    fn propagate_and_render_sinks(&self, ctx: &GpuContext) {
        let fb = self.get_output_framebuffer();
        for (sink, slot) in self.get_sinks() {
            sink.borrow_mut().set_input_framebuffer(fb, *slot, ctx);
            sink.borrow_mut().render(ctx);
        }
    }
}
