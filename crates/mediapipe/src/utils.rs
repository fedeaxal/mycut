use std::ops::Mul;
use std::path::Path;

use fast_image_resize::{Resizer, ResizeOptions};
use image::{ImageBuffer, Rgb};
use ndarray::ArrayView3;
use ort::execution_providers::ExecutionProviderDispatch;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;

use crate::geometry::Rect;

/// 检测器统一输出：坐标均为原图像素。landmarks 为检测器的粗定位点
/// （BlazeFace 6 点：右眼/左眼/鼻尖/嘴/右耳/左耳；YoloV5Face 5 点：
/// 左眼/右眼/鼻尖/左嘴角/右嘴角）。
pub struct Detection {
    pub conf: f32,
    pub bbox: Rect,
    pub landmarks: Vec<(f32, f32)>,
}

#[macro_export]
macro_rules! measure_time {
    ($block:block) => {{
        let start = std::time::Instant::now();
        let result = $block;
        let duration = start.elapsed();
        (result, duration)
    }};
}

#[allow(unused)]
pub fn save_ndarray_as_png(array: ArrayView3<f32>, path: &str) -> Result<(), image::ImageError> {
    let (_, height, width) = array.dim();
    let mut img = ImageBuffer::new(width as u32, height as u32);

    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let r = (array[[0, y as usize, x as usize]]
            .mul(255.0)
            .clamp(0., 255.)) as u8;
        let g = (array[[1, y as usize, x as usize]]
            .mul(255.0)
            .clamp(0., 255.)) as u8;
        let b = (array[[2, y as usize, x as usize]]
            .mul(255.0)
            .clamp(0., 255.)) as u8;
        *pixel = Rgb([r, g, b]);
    }

    img.save(path)
}

#[allow(unused)]
pub fn save_ndarray_as_png_t(array: ArrayView3<f32>, path: &str) -> Result<(), image::ImageError> {
    let (height, width, _) = array.dim();
    let mut img = ImageBuffer::new(width as u32, height as u32);

    for (x, y, pixel) in img.enumerate_pixels_mut() {
        let r = (array[[y as usize, x as usize, 0]]
            .mul(255.0)
            .clamp(0., 255.)) as u8;
        let g = (array[[y as usize, x as usize, 1]]
            .mul(255.0)
            .clamp(0., 255.)) as u8;
        let b = (array[[y as usize, x as usize, 2]]
            .mul(255.0)
            .clamp(0., 255.)) as u8;
        *pixel = Rgb([r, g, b]);
    }

    img.save(path)
}

#[allow(unused)]
pub fn scale_rect(r: Rect, scale: (f32, f32)) -> Rect {
    return Rect {
        xy: (r.xy.x * scale.0, r.xy.y * scale.1).into(),
        size: (r.size.width * scale.0, r.size.height * scale.1).into(),
    };
}

pub fn sigmoid(x: &f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// letterbox（contain）参数：源图等比缩放进目标画布并居中。
/// 返回 (scale_x, scale_y, off_x, off_y)：目标画布像素 -> 源图像素的逆映射为
/// `src = (canvas - off) / scale`。
pub fn letterbox_params(
    src: (u32, u32),
    dst: (u32, u32),
) -> (f32, f32, f32, f32) {
    let scale = f32::min(dst.0 as f32 / src.0 as f32, dst.1 as f32 / src.1 as f32);
    // 先取整实际绘制尺寸，再反算各轴比例，保证逆映射无累积误差
    let mid_w = ((src.0 as f32 * scale).round() as u32).max(1).min(dst.0);
    let mid_h = ((src.1 as f32 * scale).round() as u32).max(1).min(dst.1);
    (
        mid_w as f32 / src.0 as f32,
        mid_h as f32 / src.1 as f32,
        (dst.0 - mid_w) as f32 / 2.0,
        (dst.1 - mid_h) as f32 / 2.0,
    )
}

/// 执行提供者选择。Auto 按平台自动：Windows 优先 DirectML、Apple Silicon 用
/// CoreML、其余平台（Intel macOS/Linux）用 CPU。
/// 注：macOS 上 CoreML 在 Intel 核显机器实测远慢于 CPU（模型搬运开销 > 收益），
/// Intel 目标的 ort 也不编译 coreml feature。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ep {
    Auto,
    CoreML,
    DirectML,
    Cpu,
}

impl Ep {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "coreml" => Some(Self::CoreML),
            "directml" => Some(Self::DirectML),
            "cpu" => Some(Self::Cpu),
            _ => None,
        }
    }
}

/// 按平台/用户选择构建执行提供者列表（按优先级排序，末位恒为 CPU 回退）。
///
/// CoreML/DirectML 各自只在编译进 ort 对应 feature 的目标上可用（见
/// Cargo.toml 的按目标依赖表）：请求了本平台没有的 EP 时静默回退 CPU，
/// `load-dynamic` 下运行时库也未必带该 EP，让 ORT 自己再退一次。
fn execution_providers(ep: Ep) -> Vec<ExecutionProviderDispatch> {
    use ort::ep::*;
    let mut list: Vec<ExecutionProviderDispatch> = Vec::with_capacity(2);
    let wanted = match ep {
        Ep::Auto => {
            #[cfg(target_os = "windows")]
            {
                Ep::DirectML
            }
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            {
                Ep::CoreML
            }
            #[cfg(not(any(target_os = "windows", all(target_os = "macos", target_arch = "aarch64"))))]
            {
                Ep::Cpu
            }
        }
        other => other,
    };
    if wanted != Ep::Cpu {
        push_ep(&mut list, wanted);
    }
    list.push(CPU::default().build());
    list
}

/// 编译进本平台 ort feature 的 EP 才会入列；其余请求（以及 Auto 不会到达
/// 的变体）一律落回 `_`——CPU 由调用方兜底追加。
fn push_ep(list: &mut Vec<ExecutionProviderDispatch>, ep: Ep) {
    #[cfg(any(all(target_os = "macos", target_arch = "aarch64"), target_os = "windows"))]
    {
        use ort::ep::*;
        match ep {
            #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
            Ep::CoreML => list.push(CoreML::default().build()),
            #[cfg(target_os = "windows")]
            Ep::DirectML => list.push(DirectML::default().build()),
            _ => {}
        }
    }
    #[cfg(not(any(all(target_os = "macos", target_arch = "aarch64"), target_os = "windows")))]
    {
        let _ = (list, ep);
    }
}

/// 构建推理会话：sequential 执行 + intra 线程数可配。
/// 不用 ORT_PARALLEL——它为并发多请求设计，单次推理场景下与 intra 线程池
/// 争抢核心（4 核机器上尤其明显），实测更慢。
pub fn build_session<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Session> {
    let path = model.as_ref();
    // 磁盘文件缺失（打包后的可执行文件单独分发、不带 weights/ 目录）时，
    // 按文件尾名回落到编译期嵌入的模型副本
    if !path.exists() {
        if let Some(bytes) = path.file_name().and_then(|n| n.to_str()).and_then(crate::embedded::model_bytes) {
            return build_session_from_memory(bytes, ep, intra_threads);
        }
    }
    let session = session_builder(ep, intra_threads)?
        .commit_from_file(model)
        .map_err(|e| anyhow::anyhow!("[ort] commit: {e}"))?;
    Ok(session)
}

/// 从内存中的模型字节构建推理会话（onnx 格式由 ORT 内部复制解析，
/// 字节缓冲无需在会话存活期间保持有效）。
pub fn build_session_from_memory(model_bytes: &[u8], ep: Ep, intra_threads: usize) -> anyhow::Result<Session> {
    let session = session_builder(ep, intra_threads)?
        .commit_from_memory(model_bytes)
        .map_err(|e| anyhow::anyhow!("[ort] commit: {e}"))?;
    Ok(session)
}

/// 配置执行提供者与线程/优化等级的公共 builder 链，文件/内存两种加载
/// 路径共用（rc.13 的 builder 方法返回 Error<SessionBuilder>，为可 recover
/// 携带构建器自身，NonNull 使其 !Send/!Sync，不能 `?` 直转 anyhow，统一
/// map_err 成 owned 消息）。
fn session_builder(ep: Ep, intra_threads: usize) -> anyhow::Result<ort::session::builder::SessionBuilder> {
    Ok(Session::builder()
        .map_err(|e| anyhow::anyhow!("[ort] builder: {e}"))?
        .with_execution_providers(execution_providers(ep))
        .map_err(|e| anyhow::anyhow!("[ort] execution providers: {e}"))?
        .with_intra_threads(intra_threads)
        .map_err(|e| anyhow::anyhow!("[ort] intra threads: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow::anyhow!("[ort] optimization level: {e}"))?)
}

/// 模型输入/输出张量的维度（ort rc.13：Outlet → dtype().tensor_shape()）。
pub fn tensor_dims(outlet: Option<&ort::value::Outlet>) -> anyhow::Result<Vec<i64>> {
    let outlet = outlet.ok_or_else(|| anyhow::anyhow!("[ort] 模型没有该输入/输出"))?;
    let shape = outlet
        .dtype()
        .tensor_shape()
        .ok_or_else(|| anyhow::anyhow!("[ort] {} 不是张量", outlet.name()))?;
    Ok(shape.iter().copied().collect())
}

/// 延迟敏感型单次推理的默认线程数：逻辑核心数 - 2。
/// 推理期间主线程还要做解码/前后处理/输出，占满全部核心会因争抢产生抖动
/// （4 核机器实测：4 线程 47~185ms 抖动，2 线程稳定 22~32ms）。
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(2).max(1))
        .unwrap_or(2)
}

/// 检测器共用的 letterbox 预处理画布：整图等比缩放进正方形画布并居中，
/// 留边填充常量。缓冲预分配复用（逐帧零堆分配）。
pub struct LetterboxCanvas {
    pub canvas: fast_image_resize::images::Image<'static>,
    mid: fast_image_resize::images::Image<'static>,
}

impl LetterboxCanvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            canvas: fast_image_resize::images::Image::new(
                w,
                h,
                fast_image_resize::PixelType::U8x3,
            ),
            mid: fast_image_resize::images::Image::new(0, 0, fast_image_resize::PixelType::U8x3),
        }
    }

    /// 把源图 letterbox 进画布，返回 (sx, sy, ox, oy) 逆映射参数。
    pub fn fill(
        &mut self,
        src: &impl fast_image_resize::IntoImageView,
        resizer: &mut Resizer,
        fill_value: u8,
    ) -> anyhow::Result<(f32, f32, f32, f32)> {
        let src_w = src.width();
        let src_h = src.height();
        let (iw, ih) = (self.canvas.width(), self.canvas.height());
        let params @ (sx, sy, ox, oy) = letterbox_params((src_w, src_h), (iw, ih));
        let mid_w = (src_w as f32 * sx).round() as u32;
        let mid_h = (src_h as f32 * sy).round() as u32;

        if self.mid.width() != mid_w || self.mid.height() != mid_h {
            self.mid = fast_image_resize::images::Image::new(
                mid_w,
                mid_h,
                fast_image_resize::PixelType::U8x3,
            );
        }
        // 官方 ImageToTensorCalculator 用 bilinear 插值，比默认 Lanczos3 快数倍
        resizer.resize(
            src,
            &mut self.mid,
            &ResizeOptions::new().resize_alg(fast_image_resize::ResizeAlg::Interpolation(
                fast_image_resize::FilterType::Bilinear,
            )),
        )?;

        self.canvas.buffer_mut().fill(fill_value);
        {
            let (ox, oy) = (ox as usize, oy as usize);
            let (row_bytes, stride_dst) = (mid_w as usize * 3, iw as usize * 3);
            let mid_buf = self.mid.buffer();
            let dst_buf = self.canvas.buffer_mut();
            for y in 0..mid_h as usize {
                let src_row = &mid_buf[y * row_bytes..(y + 1) * row_bytes];
                let dst_row = &mut dst_buf
                    [(oy + y) * stride_dst + ox * 3..(oy + y) * stride_dst + (ox + mid_w as usize) * 3];
                dst_row.copy_from_slice(src_row);
            }
        }
        Ok(params)
    }
}
