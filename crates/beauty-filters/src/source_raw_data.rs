use wgpu::{Extent3d, TexelCopyBufferLayout, TexelCopyTextureInfo};

use crate::context::GpuContext;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;

use std::cell::RefCell;
use std::rc::Rc;

pub struct SourceRawData {
    framebuffer: Option<Framebuffer>,
    sinks: Vec<(Rc<RefCell<dyn Sink>>, u32)>,
    width: u32,
    height: u32,
}

impl SourceRawData {
    pub fn new(_ctx: &GpuContext, width: u32, height: u32) -> Self {
        Self {
            framebuffer: None,
            sinks: Vec::new(),
            width,
            height,
        }
    }

    pub fn upload_rgba(&mut self, ctx: &GpuContext, data: &[u8]) {
        self.upload(ctx, data, false);
    }

    /// BGRA 字节直接上传（Bgra8Unorm 源纹理）。wgpu 的 bgra8unorm 对
    /// 着色器通道透明，采样即得正确 RGBA 语义——无需任何换序。
    pub fn upload_bgra(&mut self, ctx: &GpuContext, data: &[u8]) {
        self.upload(ctx, data, true);
    }

    /// 带行距的上传（`bytes_per_row` >= 裸行宽）：视频帧的 plane 原样
    /// 进纹理，调用方免一次整帧重排。`data` 须有 `stride * height` 字节。
    pub fn upload_bgra_strided(&mut self, ctx: &GpuContext, data: &[u8], stride: usize) {
        let size = Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };
        let needs_new = match &self.framebuffer {
            Some(fb) => fb.width != self.width || fb.height != self.height,
            None => true,
        };
        if needs_new {
            self.framebuffer = Some(Framebuffer::new_bgra_with_copy_dst(ctx, self.width, self.height));
        }
        let fb = self.framebuffer.as_ref().unwrap();
        ctx.queue.write_texture(
            TexelCopyTextureInfo {
                texture: &fb.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(self.height),
            },
            size,
        );
    }

    fn upload(&mut self, ctx: &GpuContext, data: &[u8], bgra: bool) {
        let size = Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };

        // Reuse existing texture if dimensions match; only recreate on resize
        let needs_new = match &self.framebuffer {
            Some(fb) => fb.width != self.width || fb.height != self.height,
            None => true,
        };

        if needs_new {
            self.framebuffer = Some(if bgra {
                Framebuffer::new_bgra_with_copy_dst(ctx, self.width, self.height)
            } else {
                Framebuffer::new_with_copy_dst(ctx, self.width, self.height)
            });
        }

        let fb = self.framebuffer.as_ref().unwrap();
        ctx.queue.write_texture(
            TexelCopyTextureInfo {
                texture: &fb.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * self.width),
                rows_per_image: Some(self.height),
            },
            size,
        );
    }
}

impl Source for SourceRawData {
    fn get_output_framebuffer(&self) -> &Framebuffer {
        self.framebuffer.as_ref().expect("SourceRawData: call upload_rgba before rendering")
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
