//! 分辨率缩放滤镜:线性采样放大/缩小(全屏 quad + Filtering sampler)。
//! 用途:美颜链在低分辨率(如 360×640 → 180×320)上执行全部滤镜 pass,再由本滤镜
//! 一次性双线性放大回原分辨率——GPU 滤镜开销与像素数成正比,降采样 2x 即省 75% 像素。

use wgpu::TextureView;

use crate::context::GpuContext;
use crate::filter::FilterBase;
use crate::filter_group::FilterNode;
use crate::framebuffer::Framebuffer;
use crate::pipeline_cache::PipelineCache;
use crate::shaders::PASSTHROUGH_WGSL;
use crate::sink::Sink;
use crate::source::Source;

use std::cell::RefCell;
use std::rc::Rc;

pub struct ResizeFilter {
    base: FilterBase,
    pipeline_cache: PipelineCache,
    bind_group_layout: wgpu::BindGroupLayout,
    dummy_vb: wgpu::Buffer,
    cached_bg: Option<(TextureView, wgpu::BindGroup)>,
    out_width: u32,
    out_height: u32,
}

impl ResizeFilter {
    /// out_width/out_height:输出(放大后)分辨率;输入分辨率由 set_input_framebuffer
    /// 按上游输出自动定,渲染时仅输出尺寸固定。
    pub fn new(ctx: &GpuContext, out_width: u32, out_height: u32) -> Self {
        let bind_group_layout = FilterBase::create_bind_group_layout(ctx, false, 1);
        let dummy_vb = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("dummy vb"),
            size: 4,
            usage: wgpu::BufferUsages::VERTEX,
            mapped_at_creation: false,
        });

        Self {
            base: FilterBase::new(),
            pipeline_cache: PipelineCache::new(),
            bind_group_layout,
            dummy_vb,
            cached_bg: None,
            out_width,
            out_height,
        }
    }
}

impl FilterNode for ResizeFilter {}

impl Source for ResizeFilter {
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
        let (view, _, _) = self.base
            .input_textures
            .get(&0)
            .map(|(v, w, h)| (v.clone(), *w, *h))
            .expect("No input texture");
        // 输出固定全分辨率(构造时给定),输入任意小分辨率。
        self.base.ensure_framebuffer(ctx, self.out_width, self.out_height);

        let views: Vec<&TextureView> = [&view].to_vec();
        let bind_group = if self.cached_bg.as_ref().map(|(v, _)| v == &view).unwrap_or(false) {
            self.cached_bg.as_ref().unwrap().1.clone()
        } else {
            let bg = FilterBase::create_bind_group(
                ctx, &self.bind_group_layout, None, 0, &views,
            );
            self.cached_bg = Some((view.clone(), bg.clone()));
            bg
        };

        // passthrough shader + Filtering sampler = 双线性缩放。
        let pipeline = self.pipeline_cache.get_or_create(
            &ctx.device, "resize", PASSTHROUGH_WGSL,
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

impl Sink for ResizeFilter {
    fn set_input_framebuffer(&mut self, fb: &Framebuffer, slot: u32, _ctx: &GpuContext) {
        self.base.input_textures.insert(slot, (fb.view.clone(), fb.width, fb.height));
    }

    fn render(&mut self, ctx: &GpuContext) {
        <Self as Source>::render(self, ctx);
    }
}
