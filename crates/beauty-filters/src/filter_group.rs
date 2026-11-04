use std::cell::RefCell;
use std::rc::Rc;

use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;

/// A filter node implements both Source and Sink
pub trait FilterNode: Source + Sink {}

/// FilterGroup orchestrates rendering of multiple filters in sequence.
/// The last filter in the chain is the output. Sinks are attached to it.
///
/// Note: `get_output_framebuffer()` panics for FilterGroup. Composite filters
/// (BoxBlur, BeautyFace, etc.) must expose the output of their last internal filter
/// directly through their own Source impl, using the concrete type's Rc.
pub struct FilterGroup {
    filters: Vec<Rc<RefCell<dyn FilterNode>>>,
    empty_sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
}

impl FilterGroup {
    pub fn new() -> Self {
        Self {
            filters: Vec::new(),
            empty_sinks: vec![],
        }
    }

    pub fn add_filter(&mut self, filter: Rc<RefCell<dyn FilterNode>>) {
        self.filters.push(filter);
    }
}

impl Source for FilterGroup {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        unreachable!("FilterGroup::get_output_framebuffer - use concrete composite filter instead")
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        if let Some(last) = self.filters.last() {
            last.borrow_mut().add_sink(sink, texture_slot);
        }
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        &self.empty_sinks
    }

    fn render(&mut self, ctx: &GpuContext) {
        for filter in &self.filters {
            let mut f = filter.borrow_mut();
            Source::render(&mut *f, ctx);
            f.propagate_to_sinks(ctx);
        }
    }
}

impl Sink for FilterGroup {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, ctx: &GpuContext) {
        if let Some(first) = self.filters.first() {
            first.borrow_mut().set_input_framebuffer(fb, slot, ctx);
        }
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
