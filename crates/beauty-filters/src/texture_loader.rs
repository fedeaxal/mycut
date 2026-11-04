use wgpu::{Extent3d, TexelCopyBufferLayout, TexelCopyTextureInfo, TextureFormat};

use crate::context::GpuContext;

pub fn load_image_to_texture(ctx: &GpuContext, img: &image::DynamicImage) -> (wgpu::Texture, u32, u32) {
    let rgba = img.to_rgba8();
    let dimensions = rgba.dimensions();
    let size = Extent3d {
        width: dimensions.0,
        height: dimensions.1,
        depth_or_array_layers: 1,
    };

    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("image texture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    ctx.queue.write_texture(
        TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &rgba,
        TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(4 * dimensions.0),
            rows_per_image: Some(dimensions.1),
        },
        size,
    );

    (texture, dimensions.0, dimensions.1)
}

pub fn load_png_to_texture(ctx: &GpuContext, path: &str) -> (wgpu::Texture, u32, u32) {
    let img = image::open(path).unwrap_or_else(|e| panic!("Failed to open image {}: {}", path, e));
    load_image_to_texture(ctx, &img)
}

/// Load a PNG image from embedded bytes into a GPU texture.
/// Uses `include_bytes!` at the call site — zero filesystem I/O at runtime.
pub fn load_png_from_bytes(ctx: &GpuContext, bytes: &[u8]) -> (wgpu::Texture, u32, u32) {
    let img = image::load_from_memory(bytes).expect("Failed to decode embedded PNG");
    load_image_to_texture(ctx, &img)
}
