use std::path::Path;

use fast_image_resize::{IntoImageView, ResizeOptions, Resizer, images::Image};
use ndarray::{Array4, ArrayView, ArrayView2};
use ndarray::{Axis, s};
use ort::inputs;
use ort::value::TensorRef;
use rayon::iter::{ParallelBridge, ParallelIterator};
use tracy_client::span;

use crate::geometry::Rect;
use crate::utils::{Ep, build_session, sigmoid};

pub struct FaceLandmarkResult {
    pub points: Vec<(f32, f32, f32)>,
    pub score: f32,
    pub tongue: f32,
}

pub struct FaceLandmark {
    session: ort::session::Session,
    resizer: Resizer,
    /// 输入图像 U8x3 像素的通道顺序：true = RGB（图片解码），false = BGR。
    rgb_input: bool,
    // 预分配复用缓冲（输入尺寸固定 256×256，逐帧零分配）
    dst_img: Image<'static>,
    input_array: Array4<f32>,
}

impl FaceLandmark {
    pub const INPUT_SIZE: usize = 256;
    pub const MARKS_NUM: usize = 478;

    pub fn new<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Self> {
        let session = build_session(model, ep, intra_threads)?;
        Ok(Self {
            session,
            resizer: Resizer::new(),
            rgb_input: false,
            dst_img: Image::new(
                Self::INPUT_SIZE as u32,
                Self::INPUT_SIZE as u32,
                fast_image_resize::PixelType::U8x3,
            ),
            input_array: unsafe {
                // 注意：landmark 模型输入为 NHWC 布局
                Array4::uninit((1, Self::INPUT_SIZE, Self::INPUT_SIZE, 3)).assume_init()
            },
        })
    }

    #[must_use]
    pub fn with_rgb_input(mut self, rgb_input: bool) -> Self {
        self.rgb_input = rgb_input;
        self
    }

    pub fn proc_image(
        &mut self,
        src_image: &impl IntoImageView,
        face_rect: Rect,
        scale_to: (f32, f32),
    ) -> anyhow::Result<Option<FaceLandmarkResult>> {
        let _flm = span!("FacelandMark Face");
        _flm.emit_color(0xfe602f);
        let input_array = {
            let _preproc = span!("Pre Proc");
            // U8x3（RGB/BGR 字节序由 rgb_input 决定），缓冲区预分配复用
            self.resizer.resize(
                src_image,
                &mut self.dst_img,
                &ResizeOptions::new().crop(
                    face_rect.xy.x as f64,
                    face_rect.xy.y as f64,
                    face_rect.size.width as f64,
                    face_rect.size.height as f64,
                ),
            )?;

            let array_view = ArrayView::from_shape(
                (1, Self::INPUT_SIZE, Self::INPUT_SIZE, 3),
                self.dst_img.buffer(),
            )?;

            let rgb_array_view = if self.rgb_input {
                array_view.slice(s![.., .., .., ..])
            } else {
                array_view.slice(s![.., .., .., ..;-1])
            };
            self.input_array
                .axis_iter_mut(Axis(1))
                .zip(rgb_array_view.axis_iter(Axis(1)))
                .par_bridge()
                .for_each(|(mut new, old)| {
                    new.zip_mut_with(&old, |new, old| *new = *old as f32 / 255.0);
                });

            self.input_array.view()
        };
        let outputs = {
            let _inference = span!("Inference");
            self.session
                .run(inputs![TensorRef::from_array_view(input_array.view())?])?
        };
        {
            let _post_proc = span!("Post Proc");
            let (_, score_data) = outputs[1]
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow::anyhow!("score 输出提取失败: {e}"))?;
            let score = sigmoid(&score_data.first().copied().unwrap_or(0.0));
            if score < 0.5 {
                return Ok(None);
            }
            let (_, tongue_data) = outputs[2]
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow::anyhow!("tongue 输出提取失败: {e}"))?;
            let tongue = tongue_data.first().copied().unwrap_or(0.0);

            let (shape, data) = outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|e| anyhow::anyhow!("landmarks 输出提取失败: {e}"))?;
            anyhow::ensure!(
                shape.num_elements() == Self::MARKS_NUM * 3,
                "landmarks 输出元素数 {} != {}×3",
                shape.num_elements(),
                Self::MARKS_NUM
            );
            let marks = ArrayView2::from_shape((Self::MARKS_NUM, 3), data)
                .map_err(|e| anyhow::anyhow!("landmarks 形状转换失败: {e}"))?;
            let pos_scale = (
                scale_to.0 / Self::INPUT_SIZE as f32,
                scale_to.1 / Self::INPUT_SIZE as f32,
            );

            let points: Vec<(f32, f32, f32)> = marks
                .axis_iter(Axis(0))
                .map(|p| (pos_scale.0 * p[0], pos_scale.1 * p[1], p[2]))
                .collect();
            return Ok(Some(FaceLandmarkResult {
                points,
                score,
                tongue,
            }));
        }
    }
}
