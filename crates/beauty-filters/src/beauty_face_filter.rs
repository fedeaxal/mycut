use std::cell::RefCell;
use std::rc::Rc;

use crate::beauty_face_unit_filter::BeautyFaceUnitFilter;
use crate::box_blur_filter::BoxBlurFilter;
use crate::box_high_pass_filter::BoxHighPassFilter;
use crate::box_mono_blur_filter::radius_for_face;
use crate::context::GpuContext;
use crate::filter::CopyPass;
use crate::filter_group::{FilterGroup, FilterNode};
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;

impl FilterNode for BoxHighPassFilter {}

pub struct BeautyFaceFilter {
    group: FilterGroup,
    blur: Rc<RefCell<BoxBlurFilter>>,
    high_pass: Rc<RefCell<BoxHighPassFilter>>,
    unit: Rc<RefCell<BeautyFaceUnitFilter>>,
    output_framebuffer: Option<Framebuffer>,
    empty_sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
    copy_pass: CopyPass,
    base_radius: i32,
    /// Face width in pixels, used to auto-scale blur radius.
    /// Set via set_face_landmarks() or set_face_size().
    face_width: Option<f32>,
}

impl BeautyFaceFilter {
    pub fn new(ctx: &GpuContext, radius: i32) -> Self {
        let blur = Rc::new(RefCell::new(BoxBlurFilter::new(ctx, radius)));
        let high_pass = Rc::new(RefCell::new(BoxHighPassFilter::new(ctx, radius, 7.07)));
        let unit = Rc::new(RefCell::new(BeautyFaceUnitFilter::new(ctx)));

        // 对齐 C++ BeautyFaceFilter::Init:磨皮 blur 采样间隔 4px
        // (box_blur_filter_->SetTexelSpacingMultiplier(4));high-pass 保持默认 1。
        blur.borrow_mut().set_texel_spacing_multiplier(4.0);
        blur.borrow_mut().add_sink(unit.clone(), 1);
        high_pass.borrow_mut().add_sink(unit.clone(), 2);

        let mut group = FilterGroup::new();
        group.add_filter(blur.clone());
        group.add_filter(high_pass.clone());
        group.add_filter(unit.clone());

        Self {
            group, blur, high_pass, unit,
            output_framebuffer: None,
            empty_sinks: vec![],
            copy_pass: CopyPass::new(ctx, "copy beauty_face"),
            base_radius: radius,
            face_width: None,
        }
    }

    pub fn set_skin_smoothing(&mut self, v: f32) {
        self.unit.borrow_mut().set_blur_alpha(v);
    }

    pub fn set_sharpen(&mut self, v: f32) {
        self.unit.borrow_mut().set_sharpen(v);
    }

    pub fn set_whiteness(&mut self, v: f32) {
        self.unit.borrow_mut().set_whiten(v);
    }

    /// Set face landmarks (106 points as [x0,y0,x1,y1,...] in [0,1] normalized coords).
    /// Computes face bounding box width and uses it to auto-scale the blur radius.
    /// Call this before render() for each frame.
    pub fn set_face_landmarks(&mut self, landmarks: &[f32], img_width: u32) {
        if landmarks.len() < 2 {
            self.face_width = None;
            return;
        }
        let mut min_x = f32::MAX;
        let mut max_x = f32::MIN;
        for chunk in landmarks.chunks(2) {
            if chunk.len() == 2 {
                min_x = min_x.min(chunk[0]);
                max_x = max_x.max(chunk[0]);
            }
        }
        self.face_width = Some((max_x - min_x) * img_width as f32);
    }

    /// Explicitly set face width in pixels.
    pub fn set_face_size(&mut self, face_width: f32) {
        self.face_width = Some(face_width);
    }

    /// Compute and propagate the scaled radius to internal blur filters.
    fn update_radius(&self) {
        let radius = match self.face_width {
            Some(fw) => radius_for_face(self.base_radius, fw),
            None => self.base_radius, // fallback: use base radius as-is
        };
        self.blur.borrow().set_radius(radius);
        self.high_pass.borrow().set_radius(radius);
    }
}

impl Source for BeautyFaceFilter {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        self.output_framebuffer.as_ref().expect("BeautyFace output not ready")
    }

    fn add_sink(&mut self, sink: Rc<RefCell<dyn Sink>>, texture_slot: u32) {
        self.unit.borrow_mut().add_sink(sink, texture_slot);
    }

    fn get_sinks(&self) -> &[(Rc<RefCell<dyn Sink>>, u32)] {
        &self.empty_sinks
    }

    /// Override: sinks are stored in the internal unit filter (via add_sink delegation),
    /// but propagation must use BeautyFaceFilter's own output framebuffer (the copy).
    fn propagate_and_render_sinks(&self, ctx: &GpuContext) {
        let fb = self.output_framebuffer.as_ref().expect("BeautyFace output not ready");
        let sinks: Vec<_> = self.unit.borrow().get_sinks().to_vec();
        for (sink, slot) in &sinks {
            sink.borrow_mut().set_input_framebuffer(fb, *slot, ctx);
            sink.borrow_mut().render(ctx);
        }
    }

    fn render(&mut self, ctx: &GpuContext) {
        self.update_radius();
        Source::render(&mut self.group, ctx);
        let (w, h) = {
            let u = self.unit.borrow();
            let fb = u.get_output_framebuffer();
            (fb.width, fb.height)
        };
        let needs_new = match &self.output_framebuffer {
            Some(fb) => fb.width != w || fb.height != h,
            None => true,
        };
        if needs_new {
            self.output_framebuffer = Some(Framebuffer::new(ctx, w, h));
        }
        let src_view = self.unit.borrow().get_output_framebuffer().view.clone();
        self.copy_pass.draw(ctx, &src_view, &self.output_framebuffer.as_ref().unwrap().view);
        self.propagate_and_render_sinks(ctx);
    }
}

impl Sink for BeautyFaceFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, _slot: u32, ctx: &GpuContext) {
        self.unit.borrow_mut().set_input_framebuffer(fb, 0, ctx);
        self.blur.borrow_mut().set_input_framebuffer(fb, 0, ctx);
        self.high_pass.borrow_mut().set_input_framebuffer(fb, 0, ctx);
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}

impl FilterNode for BeautyFaceFilter {}
