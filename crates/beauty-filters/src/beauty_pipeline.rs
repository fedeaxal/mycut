use std::cell::RefCell;
use std::rc::Rc;

use crate::beauty_face_filter::BeautyFaceFilter;
use crate::blusher_filter::BlusherFilter;
use crate::context::GpuContext;
use crate::face_reshape_filter::FaceReshapeFilter;
use crate::filter::CopyPass;
use crate::framebuffer::Framebuffer;
use crate::sink::Sink;
use crate::source::Source;
use crate::source_raw_data::SourceRawData;

/// 美颜参数（0..1，全 0 = 直通）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeautyParams {
    pub smoothing: f32, // 磨皮
    pub whiten: f32,    // 美白
    pub face_slim: f32, // 瘦脸
    pub eye_zoom: f32,  // 大眼
    pub blusher: f32,   // 腮红
}

impl Default for BeautyParams {
    fn default() -> Self {
        Self { smoothing: 0.0, whiten: 0.0, face_slim: 0.0, eye_zoom: 0.0, blusher: 0.0 }
    }
}

/// 输入/输出的字节序。视频链路（VideoToolbox/D3D11 像素缓冲）天然 BGRA：
/// BGRA 字节上传进 Bgra8Unorm 源纹理后，shader 采到的就是正确 RGBA 语义
/// （wgpu 的 bgra8unorm 对着色器通道透明，egui 在 Bgra8Unorm 表面上直接
/// 画 UI 即证），输出 framebuffer 也用 Bgra8Unorm——两次显存的通道重排
/// 由纹理格式各自完成、互相抵消，CPU 与 shader 全程零换序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Rgba,
    Bgra,
}

/// 固定顺序的 GPU 美颜链：SourceRawData(CPU 上传) → [磨皮美白 → 瘦脸大眼 → 腮红] → 输出纹理。
/// 链全程留在 GPU，输出纹理可直接被合成器采样（零回读）。
/// 三组滤镜始终在链上（参数 0 时 shader 直通，pass 开销 ~0.5ms/帧 @1080p），
/// 换取参数实时变化时无需整链重建。
pub struct BeautyPipeline {
    ctx: GpuContext,
    format: SourceFormat,
    width: u32,
    height: u32,
    source: SourceRawData,
    beauty: Rc<RefCell<BeautyFaceFilter>>,
    reshape: Rc<RefCell<FaceReshapeFilter>>,
    blusher: Rc<RefCell<BlusherFilter>>,
    output: Framebuffer,
    copy: CopyPass,
    params: BeautyParams,
    /// 上次交给 reshape/blusher 的 landmark 快照。set_face_landmarks 按值收 Vec<f32>，
    /// 未变化时跳过调用，稳定帧零分配（避免每帧 2 次 212 f32 的 to_vec）。
    last_makeup: Vec<f32>,
    last_reshape: Vec<f32>,
}

impl BeautyPipeline {
    pub fn new(ctx: GpuContext, width: u32, height: u32) -> Self {
        Self::with_format(ctx, width, height, SourceFormat::Rgba)
    }

    /// BGRA 进出（`process_bgra` + `read_output_bgra`）的管线。
    pub fn new_bgra(ctx: GpuContext, width: u32, height: u32) -> Self {
        Self::with_format(ctx, width, height, SourceFormat::Bgra)
    }

    pub fn with_format(ctx: GpuContext, width: u32, height: u32, format: SourceFormat) -> Self {
        let beauty = Rc::new(RefCell::new(BeautyFaceFilter::new(&ctx, 4)));
        let reshape = Rc::new(RefCell::new(FaceReshapeFilter::new(&ctx)));
        let blusher = Rc::new(RefCell::new(BlusherFilter::new(&ctx)));
        let mut source = SourceRawData::new(&ctx, width, height);
        // BGRA 与 RGBA 模式的链形状相同：Bgra8Unorm 的采样/写回对 shader
        // 通道透明，无需任何换序 pass（曾在此放一个 swap pass，它才是
        // 输出红蓝反转的原因——见 SourceFormat 的文档）。
        source.add_sink(beauty.clone() as Rc<RefCell<dyn Sink>>, 0);
        beauty.borrow_mut().add_sink(reshape.clone() as Rc<RefCell<dyn Sink>>, 0);
        reshape.borrow_mut().add_sink(blusher.clone() as Rc<RefCell<dyn Sink>>, 0);
        let output = match format {
            // BGRA 输出：最终 copy 写 Bgra8Unorm，回读字节即为 BGRA。
            SourceFormat::Bgra => Framebuffer::new_bgra_output(&ctx, width, height),
            SourceFormat::Rgba => Framebuffer::new(&ctx, width, height),
        };
        let copy = match format {
            SourceFormat::Bgra => CopyPass::new_with_target_format(
                &ctx,
                "beauty pipeline output",
                wgpu::TextureFormat::Bgra8Unorm,
            ),
            SourceFormat::Rgba => CopyPass::new(&ctx, "beauty pipeline output"),
        };
        Self {
            ctx,
            format,
            width,
            height,
            source,
            beauty,
            reshape,
            blusher,
            output,
            copy,
            params: BeautyParams::default(),
            last_makeup: Vec::new(),
            last_reshape: Vec::new(),
        }
    }

    pub fn set_params(&mut self, params: BeautyParams) {
        self.params = params;
    }

    /// 输出纹理尺寸（抓拍回读用）。
    pub fn width(&self) -> u32 {
        self.output.width
    }

    pub fn height(&self) -> u32 {
        self.output.height
    }

    /// 处理一帧 CPU RGBA。两个 landmark 数组均由桌面侧从 478 点映射而来：
    /// - `makeup`：106 点 [x0,y0,...]（212 f32，归一化）——磨皮 bbox 自适应半径 + 腮红网格；
    /// - `reshape`：17 控制点 [x0,y0,...]（212 f32，其余为 0）——瘦脸/大眼 delta 域；
    /// 两者为空切片 = 无脸（reshape/blusher 直通，仅磨皮/美白仍生效）。
    /// 返回输出纹理（与 ctx 同 device，可被外部直接采样）。
    pub fn process(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        makeup: &[f32],
        reshape: &[f32],
    ) -> &wgpu::Texture {
        debug_assert_eq!(self.format, SourceFormat::Rgba, "use process_bgra on a BGRA pipeline");
        self.process_inner(rgba, width, height, makeup, reshape)
    }

    /// BGRA 管线的一帧：`bgra` 为 Bgra8Unorm 字节序（视频像素缓冲的布局）。
    pub fn process_bgra(
        &mut self,
        bgra: &[u8],
        width: u32,
        height: u32,
        makeup: &[f32],
        reshape: &[f32],
    ) -> &wgpu::Texture {
        debug_assert_eq!(self.format, SourceFormat::Bgra, "use process on an RGBA pipeline");
        self.process_inner(bgra, width, height, makeup, reshape)
    }

    /// 同 [`Self::process_bgra`]，但 `bgra` 带行距（视频帧 plane 原样上传，
    /// 免整帧重排；须有 `stride * height` 字节）。
    pub fn process_bgra_strided(
        &mut self,
        bgra: &[u8],
        stride: usize,
        width: u32,
        height: u32,
        makeup: &[f32],
        reshape: &[f32],
    ) -> &wgpu::Texture {
        debug_assert_eq!(self.format, SourceFormat::Bgra, "use process on an RGBA pipeline");
        if width != self.width || height != self.height {
            self.resize(width, height);
        }
        self.apply_params(makeup, reshape);
        self.source.upload_bgra_strided(&self.ctx, bgra, stride);
        self.ctx.begin_frame();
        Source::render(&mut self.source, &self.ctx);
        let src_view = self.blusher.borrow().get_output_framebuffer().view.clone();
        self.copy.draw(&self.ctx, &src_view, &self.output.view);
        self.ctx.end_frame();
        &self.output.texture
    }

    fn process_inner(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        makeup: &[f32],
        reshape: &[f32],
    ) -> &wgpu::Texture {
        if width != self.width || height != self.height {
            self.resize(width, height);
        }
        self.apply_params(makeup, reshape);
        match self.format {
            SourceFormat::Bgra => self.source.upload_bgra(&self.ctx, pixels),
            SourceFormat::Rgba => self.source.upload_rgba(&self.ctx, pixels),
        }
        self.submit();
        &self.output.texture
    }

    /// 尺寸变化：重建输入源与输出 framebuffer（各滤镜内部 ensure_framebuffer 自动跟随）。
    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.source = SourceRawData::new(&self.ctx, width, height);
        self.source
            .add_sink(self.beauty.clone() as Rc<RefCell<dyn Sink>>, 0);
        self.output = match self.format {
            SourceFormat::Bgra => Framebuffer::new_bgra_output(&self.ctx, width, height),
            SourceFormat::Rgba => Framebuffer::new(&self.ctx, width, height),
        };
    }

    /// 把参数与地标交给链上各滤镜（landmark 未变化时零分配直跳）。
    fn apply_params(&mut self, makeup: &[f32], reshape: &[f32]) {
        let width = self.width;
        let has_face = makeup.len() >= 212;
        // 磨皮：bbox 宽自适应半径（BeautyFaceFilter 内部由 set_face_landmarks 计算）。
        self.beauty.borrow_mut().set_face_landmarks(makeup, width);
        self.beauty.borrow_mut().set_skin_smoothing(self.params.smoothing);
        self.beauty.borrow_mut().set_whiteness(self.params.whiten);
        if has_face {
            if self.last_reshape.as_slice() != reshape {
                // 传新值给滤镜并同步快照（不能 mem::replace 传旧值）。
                self.last_reshape = reshape.to_vec();
                self.reshape
                    .borrow_mut()
                    .set_face_landmarks(self.last_reshape.clone());
            }
            // 0..1 → delta 域缩放（OpenCut 真机校准）。
            self.reshape.borrow_mut().set_face_slim_level(self.params.face_slim * 0.6);
            self.reshape.borrow_mut().set_eye_zoom_level(self.params.eye_zoom * 0.5);
        } else if !self.last_reshape.is_empty() {
            // 无脸：只在该传递时传空切片（滤镜 has_face=false 直通），并同步快照为空。
            self.last_reshape.clear();
            self.reshape
                .borrow_mut()
                .set_face_landmarks(Vec::new());
        }
        self.blusher.borrow_mut().set_intensity(self.params.blusher);
        if has_face {
            if self.last_makeup.as_slice() != makeup {
                self.last_makeup = makeup.to_vec();
                self.blusher
                    .borrow_mut()
                    .set_face_landmarks(self.last_makeup.clone());
            }
        } else if !self.last_makeup.is_empty() {
            self.last_makeup.clear();
            self.blusher
                .borrow_mut()
                .set_face_landmarks(Vec::new());
        }
    }

    /// 提交一帧：上传已由调用方完成。
    fn submit(&mut self) {
        self.ctx.begin_frame();
        Source::render(&mut self.source, &self.ctx);
        let src_view = self.blusher.borrow().get_output_framebuffer().view.clone();
        self.copy.draw(&self.ctx, &src_view, &self.output.view);
        self.ctx.end_frame();
    }

    /// 抓拍用：同步回读当前输出纹理为 CPU RGBA（低频调用）。
    pub fn read_output_rgba(&mut self) -> Vec<u8> {
        debug_assert_eq!(self.format, SourceFormat::Rgba);
        self.read_output_bytes()
    }

    /// BGRA 管线的回读：字节即为 Bgra8Unorm 布局。
    pub fn read_output_bgra(&mut self) -> Vec<u8> {
        debug_assert_eq!(self.format, SourceFormat::Bgra);
        self.read_output_bytes()
    }

    fn read_output_bytes(&mut self) -> Vec<u8> {
        let (w, h) = (self.output.width, self.output.height);
        let bytes_per_row = w * 4;
        // copy_texture_to_buffer 要求每行对齐 256 字节（1080p 的 4320 不满足 4w 裸行宽）。
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = (bytes_per_row + align - 1) / align * align;
        let buffer = self.ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("beauty readback"),
            size: (padded as u64) * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("beauty readback enc"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.output.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let idx = self.ctx.queue.submit(Some(encoder.finish()));
        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        // 与 SinkRawData::read_pixels 同款有界轮询：Wait 变体带 timeout + submission_index，
        // 避免 wgpu 29 裸 `PollType::Wait`（struct variant 必须带字段）。
        let poll_ok = self
            .ctx
            .device
            .poll(wgpu::PollType::Wait {
                timeout: Some(std::time::Duration::from_secs(3)),
                submission_index: Some(idx),
            })
            .is_ok();
        let mapped = poll_ok
            && matches!(
                rx.recv_timeout(std::time::Duration::from_millis(500)),
                Ok(Ok(()))
            );
        if !mapped {
            return Vec::new();
        }
        let mut result = Vec::with_capacity((w as usize) * (h as usize) * 4);
        {
            let mapped_range = slice.get_mapped_range().expect("mapped in");
            for chunk in mapped_range.chunks(padded as usize) {
                result.extend_from_slice(&chunk[..bytes_per_row as usize]);
            }
        }
        buffer.unmap();
        result
    }
}
