use std::cell::RefCell;
use std::rc::Rc;

use crate::box_blur_filter::BoxBlurFilter;
use crate::box_difference_filter::BoxDifferenceFilter;
use crate::context::GpuContext;
use crate::filter::CopyPass;
use crate::filter_group::{FilterGroup, FilterNode};
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;

impl FilterNode for BoxBlurFilter {}
impl FilterNode for BoxDifferenceFilter {}

pub struct BoxHighPassFilter {
    group: FilterGroup,
    blur: Rc<RefCell<BoxBlurFilter>>,
    diff: Rc<RefCell<BoxDifferenceFilter>>,
    output_framebuffer: Option<Framebuffer>,
    empty_sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
    copy_pass: CopyPass,
}

impl BoxHighPassFilter {
    pub fn new(ctx: &GpuContext, radius: i32, delta: f32) -> Self {
        let blur = Rc::new(RefCell::new(BoxBlurFilter::new(ctx, radius)));
        let diff = Rc::new(RefCell::new(BoxDifferenceFilter::new(ctx)));
        diff.borrow_mut().set_delta(delta);

        blur.borrow_mut().add_sink(diff.clone(), 1);

        let mut group = FilterGroup::new();
        group.add_filter(blur.clone());
        group.add_filter(diff.clone());

        Self {
            group, blur, diff,
            output_framebuffer: None,
            empty_sinks: vec![],
            copy_pass: CopyPass::new(ctx, "copy box_high_pass"),
        }
    }

    pub fn set_radius(&self, radius: i32) {
        self.blur.borrow().set_radius(radius);
    }
}

impl Source for BoxHighPassFilter {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        self.output_framebuffer.as_ref().expect("BoxHighPass output not ready")
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        self.diff.borrow_mut().add_sink(sink, texture_slot);
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        &self.empty_sinks
    }

    fn render(&mut self, ctx: &GpuContext) {
        Source::render(&mut self.group, ctx);
        let (w, h) = {
            let d = self.diff.borrow();
            let fb = d.get_output_framebuffer();
            (fb.width, fb.height)
        };
        let needs_new = match &self.output_framebuffer {
            Some(fb) => fb.width != w || fb.height != h,
            None => true,
        };
        if needs_new {
            self.output_framebuffer = Some(Framebuffer::new(ctx, w, h));
        }
        let src_view = self.diff.borrow().get_output_framebuffer().view.clone();
        self.copy_pass.draw(ctx, &src_view, &self.output_framebuffer.as_ref().unwrap().view);
        // 只传播不递归渲染下游(与 BoxBlurFilter 同因:被 BeautyFaceFilter group 复用,
        // 递归会重复渲染 unit 及整条下游链)。
        self.propagate_to_sinks(ctx);
    }
}

impl Sink for BoxHighPassFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, _slot: u32, ctx: &GpuContext) {
        self.diff.borrow_mut().set_input_framebuffer(fb, 0, ctx);
        self.blur.borrow_mut().set_input_framebuffer(fb, 0, ctx);
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
