use wgpu::{Extent3d, Texture, TextureFormat, TextureView};

use crate::context::GpuContext;

pub struct Framebuffer {
    pub texture: Texture,
    pub view: TextureView,
    pub width: u32,
    pub height: u32,
}

impl Framebuffer {
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Self {
        Self::new_with_format(ctx, width, height, TextureFormat::Rgba8Unorm)
    }

    pub fn new_with_format(ctx: &GpuContext, width: u32, height: u32, format: TextureFormat) -> Self {
        // COPY_SRC: 输出纹理可被 copy_texture_to_buffer 回读（BeautyPipeline::read_output_rgba 依赖）。
        Self::new_with_usage(ctx, width, height, format,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC)
    }

    /// Create a framebuffer whose texture also supports COPY_DST for write_texture uploads.
    pub fn new_with_copy_dst(ctx: &GpuContext, width: u32, height: u32) -> Self {
        Self::new_with_usage(ctx, width, height, TextureFormat::Rgba8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST)
    }

    /// BGRA 输入源：write_texture 直接落 BGRA 字节（视频链路免 CPU 换序）。
    pub fn new_bgra_with_copy_dst(ctx: &GpuContext, width: u32, height: u32) -> Self {
        Self::new_with_usage(ctx, width, height, TextureFormat::Bgra8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST)
    }

    /// BGRA 输出：回读得到的字节即为 BGRA（管线内部仍以 RGBA 语义渲染）。
    pub fn new_bgra_output(ctx: &GpuContext, width: u32, height: u32) -> Self {
        Self::new_with_usage(ctx, width, height, TextureFormat::Bgra8Unorm,
            wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC)
    }

    fn new_with_usage(ctx: &GpuContext, width: u32, height: u32, format: TextureFormat, usage: wgpu::TextureUsages) -> Self {
        let texture = ctx
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(&format!("framebuffer {}x{}", width, height)),
                size: Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            texture,
            view,
            width,
            height,
        }
    }
}
