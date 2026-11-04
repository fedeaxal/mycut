//! MediaPipe SelfieSegmenter 人像分割（官方 selfie_segmenter 模型，square 256 变体）。
//!
//! 预处理与官方 ImageSegmenter 一致：整图直接缩放到 256×256（不保持宽高比），
//! 像素归一化到 [0,1]。模型输出 alpha 掩码 [1,1,256,256]，取值 0~1（已含 sigmoid，
//! 数值域经实测确认）。
//!
//! 说明：官方 tflite 使用 mediapipe 专有自定义算子 `Convolution2DTransposeBias`，
//! TFLite/ONNX 均无法直接消费；本模块使用其社区 ONNX 转换版
//! （weights/selfie_segmenter.onnx，输入 pixel_values NCHW，输出 alphas）。
//!
//! 另提供 [`MulticlassSegmenter`]：官方 selfie_multiclass_256x256 模型的转换版
//! （weights/selfie_multiclass_256x256.onnx，6 类逐像素分类，人像质量高于
//! selfie_segmenter）。实测其输出通道 0 为 background 的 logit，
//! 人像掩码 = 1 − softmax(background)，其余通道语义未逐一校准。

use std::path::Path;

use fast_image_resize::{IntoImageView, PixelType, Resizer, ResizeAlg, ResizeOptions, FilterType};
use ndarray::{Array4, ArrayView, ArrayView2, ArrayView3, Axis};
use ort::inputs;
use ort::value::TensorRef;
use rayon::iter::{ParallelBridge, ParallelIterator};

use tracy_client::span;

use crate::utils::{Ep, build_session};

/// 模型输入/掩码边长
pub const MASK_SIZE: usize = 256;

/// 人像 alpha 掩码：row-major 的 [height, width] 浮点图，取值 0~1（1 = 人像）。
pub struct SegmentationMask {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl SegmentationMask {
    /// 放大到目标尺寸并转 8 位灰度（行优先）。
    /// 在浮点空间用 CatmullRom 卷积插值放大后再量化——若先量化成 8bit 再放大，
    /// 边缘的量化阶梯会被插值放大，产生明显的锯齿。
    pub fn to_gray_resized(&self, width: u32, height: u32) -> image::GrayImage {
        let src_buf: Vec<u8> = self.data.iter().flat_map(|v| v.to_ne_bytes()).collect();
        let src = fast_image_resize::images::Image::from_vec_u8(
            self.width as u32,
            self.height as u32,
            src_buf,
            PixelType::F32,
        )
        .expect("mask size mismatch");
        let mut resizer = Resizer::new();
        let mut dst = fast_image_resize::images::Image::new(width, height, PixelType::F32);
        resizer
            .resize(
                &src,
                &mut dst,
                &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom)),
            )
            .expect("mask resize failed");
        let f: &[f32] = f32_slice(dst.buffer());
        image::GrayImage::from_fn(width, height, |x, y| {
            let v = f[(y as usize) * width as usize + x as usize];
            image::Luma([(v.clamp(0.0, 1.0) * 255.0).round() as u8])
        })
    }
}

/// F32 像素缓冲（以 Vec<u8> 存储的 little-endian f32）转 f32 切片。
fn f32_slice(buf: &[u8]) -> &[f32] {
    debug_assert!(buf.len() % 4 == 0);
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<f32>(), buf.len() / 4) }
}

pub struct SelfieSegmenter {
    session: ort::session::Session,
    resizer: Resizer,
    /// 预分配复用缓冲（逐帧零分配）
    img_buf: fast_image_resize::images::Image<'static>,
    input_array: Array4<f32>,
}

impl SelfieSegmenter {
    pub fn new<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Self> {
        let session = build_session(model, ep, intra_threads)?;
        let dims = crate::utils::tensor_dims(session.inputs().first())?;
        anyhow::ensure!(
            dims.len() == 4 && dims[1] == 3 && dims[2] == MASK_SIZE as i64 && dims[3] == MASK_SIZE as i64,
            "SelfieSegmenter 模型输入应为 [N,3,{MASK_SIZE},{MASK_SIZE}]，实际为 {dims:?}"
        );
        Ok(Self {
            session,
            resizer: Resizer::new(),
            img_buf: fast_image_resize::images::Image::new(
                MASK_SIZE as u32,
                MASK_SIZE as u32,
                PixelType::U8x3,
            ),
            input_array: unsafe {
                Array4::uninit((1, 3, MASK_SIZE, MASK_SIZE)).assume_init()
            },
        })
    }

    /// 分割输入图像，返回 256×256 的 alpha 掩码（0~1）。
    pub fn proc_image(&mut self, src_image: &impl IntoImageView) -> anyhow::Result<SegmentationMask> {
        let _seg = span!("SelfieSegmenter");
        _seg.emit_color(0x2fa02f);

        // ---- 预处理：整图缩放 256×256 + /255（缓冲复用，NCHW）----
        // Convolution(Bilinear)：降采样时卷积核随缩放比扩展（抗锯齿），
        // 与 PIL/OpenCV 的抗锯齿缩放等价；无抗锯齿的插值在大倍率缩小时会走样伤分割质量
        let raw_img = {
            let _pre = span!("Pre Proc");
            self.resizer.resize(
                src_image,
                &mut self.img_buf,
                &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
            )?;
            let view = ArrayView::from_shape(
                (1, MASK_SIZE, MASK_SIZE, 3),
                self.img_buf.buffer(),
            )?
            .permuted_axes([0, 3, 1, 2]); // [1,h,w,3] -> [1,3,h,w]

            let out = &mut self.input_array;
            out.axis_iter_mut(Axis(1))
                .zip(view.axis_iter(Axis(1)))
                .par_bridge()
                .for_each(|(mut new, old)| {
                    new.zip_mut_with(&old, |new, old| *new = *old as f32 / 255.0);
                });
            out.view()
        };

        // ---- 推理 ----
        let outputs = {
            let _inf = span!("Inference");
            self.session
                .run(inputs![TensorRef::from_array_view(raw_img)?])?
        };

        // ---- 输出掩码 ----
        let _post = span!("Post Proc");
        // 按形状识别 alphas 输出 [N,1,256,256]
        let alpha_idx = outputs
            .iter()
            .position(|(_, o)| {
                o.try_extract_tensor::<f32>().map(|(shape, _)| {
                    let dims: Vec<i64> = shape.iter().copied().collect();
                    dims.len() == 4 && dims[2] == MASK_SIZE as i64 && dims[3] == MASK_SIZE as i64
                }).unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("SelfieSegmenter 输出形状异常: 未找到 {MASK_SIZE}×{MASK_SIZE} 掩码"))?;
        let (_, alpha) = outputs[alpha_idx].try_extract_tensor::<f32>()?;
        let mask = ArrayView2::from_shape((MASK_SIZE, MASK_SIZE), alpha)
            .map_err(|e| anyhow::anyhow!("SelfieSegmenter 掩码形状不匹配: {e}"))?;
        Ok(SegmentationMask {
            width: MASK_SIZE,
            height: MASK_SIZE,
            data: mask.iter().copied().collect(),
        })
    }
}

/// SelfieSegmenter 的类别/变体选择。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SegmenterKind {
    /// selfie_segmenter（默认，462KB，最快）
    Selfie,
    /// selfie_multiclass_256x256（16MB，6 类逐像素分类合并为人像掩码，质量更高）
    Multiclass,
}

impl SegmenterKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "selfie" => Some(Self::Selfie),
            "multiclass" => Some(Self::Multiclass),
            _ => None,
        }
    }

    pub fn model_file(self) -> &'static str {
        match self {
            Self::Selfie => "selfie_segmenter.onnx",
            Self::Multiclass => "selfie_multiclass_256x256.onnx",
        }
    }
}

/// selfie_multiclass_256x256 分割器（NHWC 输入，输出 6 类 logits）。
pub struct MulticlassSegmenter {
    session: ort::session::Session,
    resizer: Resizer,
    img_buf: fast_image_resize::images::Image<'static>,
    input_array: Array4<f32>,
}

impl MulticlassSegmenter {
    pub fn new<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Self> {
        let session = build_session(model, ep, intra_threads)?;
        let dims = crate::utils::tensor_dims(session.inputs().first())?;
        anyhow::ensure!(
            dims.len() == 4
                && dims[1] == MASK_SIZE as i64
                && dims[2] == MASK_SIZE as i64
                && dims[3] == 3,
            "MulticlassSegmenter 模型输入应为 [N,{MASK_SIZE},{MASK_SIZE},3]，实际为 {dims:?}"
        );
        Ok(Self {
            session,
            resizer: Resizer::new(),
            img_buf: fast_image_resize::images::Image::new(
                MASK_SIZE as u32,
                MASK_SIZE as u32,
                PixelType::U8x3,
            ),
            input_array: unsafe {
                Array4::uninit((1, MASK_SIZE, MASK_SIZE, 3)).assume_init()
            },
        })
    }

    /// 分割输入图像，返回 256×256 的人像 alpha 掩码（0~1）。
    pub fn proc_image(&mut self, src_image: &impl IntoImageView) -> anyhow::Result<SegmentationMask> {
        let _seg = span!("MulticlassSegmenter");
        _seg.emit_color(0x2fa02f);

        let raw_img = {
            let _pre = span!("Pre Proc");
            self.resizer.resize(
                src_image,
                &mut self.img_buf,
                &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
            )?;
            let out = &mut self.input_array;
            let view = ArrayView::from_shape(
                (1, MASK_SIZE, MASK_SIZE, 3),
                self.img_buf.buffer(),
            )?;
            out.axis_iter_mut(Axis(0))
                .zip(view.axis_iter(Axis(0)))
                .par_bridge()
                .for_each(|(mut new, old)| {
                    new.zip_mut_with(&old, |new, old| *new = *old as f32 / 255.0);
                });
            out.view()
        };

        let outputs = {
            let _inf = span!("Inference");
            self.session
                .run(inputs![TensorRef::from_array_view(raw_img)?])?
        };

        let _post = span!("Post Proc");
        // 识别 [N,256,256,6] 的 logits 输出
        let logits_idx = outputs
            .iter()
            .position(|(_, o)| {
                o.try_extract_tensor::<f32>().map(|(shape, _)| {
                    let dims: Vec<i64> = shape.iter().copied().collect();
                    dims.len() == 4 && dims[2] == MASK_SIZE as i64 && dims[3] == 6
                }).unwrap_or(false)
            })
            .ok_or_else(|| anyhow::anyhow!("MulticlassSegmenter 输出形状异常: 未找到 [N,256,256,6] logits"))?;
        let (_, logits) = outputs[logits_idx].try_extract_tensor::<f32>()?;
        let raw = ArrayView3::from_shape((MASK_SIZE, MASK_SIZE, 6), logits)
            .map_err(|e| anyhow::anyhow!("MulticlassSegmenter logits 形状不匹配: {e}"))?;

        // 人像概率 = 1 - softmax(background)；背景通道 0 已实测确认
        let mut data = Vec::with_capacity(MASK_SIZE * MASK_SIZE);
        for px in raw.rows() {
            let max = px.iter().copied().fold(f32::MIN, f32::max);
            let exp_sum: f32 = px.iter().map(|&v| (v - max).exp()).sum();
            let p_bg = (px[0] - max).exp() / exp_sum;
            data.push(1.0 - p_bg);
        }
        Ok(SegmentationMask {
            width: MASK_SIZE,
            height: MASK_SIZE,
            data,
        })
    }
}
