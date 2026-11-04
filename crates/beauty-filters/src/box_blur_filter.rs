use std::cell::RefCell;
use std::rc::Rc;

use crate::box_mono_blur_filter::{BlurDirection, BoxMonoBlurFilter};
use crate::context::GpuContext;
use crate::filter::CopyPass;
use crate::filter_group::{FilterGroup, FilterNode};
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;

impl FilterNode for BoxMonoBlurFilter {}

pub struct BoxBlurFilter {
    group: FilterGroup,
    h_blur: Rc<RefCell<BoxMonoBlurFilter>>,
    v_blur: Rc<RefCell<BoxMonoBlurFilter>>,
    output_framebuffer: Option<Framebuffer>,
    empty_sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
    copy_pass: CopyPass,
}

impl BoxBlurFilter {
    pub fn new(ctx: &GpuContext, radius: i32) -> Self {
        let h_blur = Rc::new(RefCell::new(BoxMonoBlurFilter::new(ctx, BlurDirection::Horizontal, radius)));
        let v_blur = Rc::new(RefCell::new(BoxMonoBlurFilter::new(ctx, BlurDirection::Vertical, radius)));

        h_blur.borrow_mut().add_sink(v_blur.clone(), 0);

        let mut group = FilterGroup::new();
        group.add_filter(h_blur.clone());
        group.add_filter(v_blur.clone());

        Self {
            group, h_blur, v_blur,
            output_framebuffer: None,
            empty_sinks: vec![],
            copy_pass: CopyPass::new(ctx, "copy box_blur"),
        }
    }

    pub fn set_radius(&self, radius: i32) {
        self.h_blur.borrow_mut().set_radius(radius);
        self.v_blur.borrow_mut().set_radius(radius);
    }

    /// Set sampling interval in pixels for both directions
    /// (C++ BoxBlurFilter::SetTexelSpacingMultiplier).
    pub fn set_texel_spacing_multiplier(&self, spacing: f32) {
        self.h_blur.borrow_mut().set_texel_spacing_multiplier(spacing);
        self.v_blur.borrow_mut().set_texel_spacing_multiplier(spacing);
    }
}

impl Source for BoxBlurFilter {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        self.output_framebuffer.as_ref().expect("BoxBlur output not ready")
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        self.v_blur.borrow_mut().add_sink(sink, texture_slot);
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        &self.empty_sinks
    }

    fn render(&mut self, ctx: &GpuContext) {
        Source::render(&mut self.group, ctx);
        let (w, h) = {
            let v = self.v_blur.borrow();
            let fb = v.get_output_framebuffer();
            (fb.width, fb.height)
        };
        let needs_new = match &self.output_framebuffer {
            Some(fb) => fb.width != w || fb.height != h,
            None => true,
        };
        if needs_new {
            self.output_framebuffer = Some(Framebuffer::new(ctx, w, h));
        }
        let src_view = {
            let v = self.v_blur.borrow();
            v.get_output_framebuffer().view.clone()
        };
        self.copy_pass.draw(ctx, &src_view, &self.output_framebuffer.as_ref().unwrap().view);
        // 只传播不递归渲染下游:BoxBlurFilter 在 BeautyFaceFilter 内部 group 中被
        // 复用(磨皮 blur + high-pass blur),propagate_and_render_sinks 会把整条下游
        // (unit→reshape→blusher→lipstick→sink)重复渲染多次。下游渲染统一由链入口
        // BeautyFaceFilter::render 驱动一次(见 beauty_face_filter.rs)。
        self.propagate_to_sinks(ctx);
    }
}

impl Sink for BoxBlurFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, ctx: &GpuContext) {
        self.h_blur.borrow_mut().set_input_framebuffer(fb, slot, ctx);
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
