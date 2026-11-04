use fast_image_resize::{IntoImageView, Resizer};
use ndarray::{Array4, ArrayView, ArrayView3};
use ndarray::{Axis, s};
use ort::inputs;
use ort::value::TensorRef;
use rayon::iter::{ParallelBridge, ParallelIterator};
use std::path::Path;

use tracy_client::span;

use crate::geometry::Rect;
use crate::utils::{Detection, Ep, LetterboxCanvas, build_session};

mod result;

use result::YoloBox;

pub struct YoloV5Face {
    session: ort::session::Session,
    resizer: Resizer,
    pub input_shape: (usize, usize),
    #[allow(unused)]
    pub output_shape: (usize, usize, usize),
    pub pos_scale: (f32, f32),
    /// 输入图像 U8x3 像素的通道顺序：true = RGB（图片解码），false = BGR。
    /// 仅影响预处理切片的通道取值方向，不引入额外内存遍历。
    rgb_input: bool,
    letterbox: LetterboxCanvas,
    input_array: Array4<f32>,
}

impl YoloV5Face {
    pub fn new<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Self> {
        let session = build_session(model, ep, intra_threads)?;

        let dims = crate::utils::tensor_dims(session.inputs().first())?;
        let dims = &dims[2..];
        let input_shape = (dims[0] as usize, dims[1] as usize);
        let dims = crate::utils::tensor_dims(session.outputs().first())?;
        let output_shape = (dims[0] as usize, dims[1] as usize, dims[2] as usize);
        let pos_scale = (1.0 / input_shape.0 as f32, 1.0 / input_shape.1 as f32);

        Ok(Self {
            session,
            input_shape,
            output_shape,
            resizer: Resizer::new(),
            pos_scale,
            rgb_input: false,
            letterbox: LetterboxCanvas::new(input_shape.0 as u32, input_shape.1 as u32),
            input_array: unsafe {
                Array4::uninit((1, 3, input_shape.0, input_shape.1)).assume_init()
            },
        })
    }

    #[must_use]
    pub fn with_rgb_input(mut self, rgb_input: bool) -> Self {
        self.rgb_input = rgb_input;
        self
    }

    /// 检测人脸，输出原图像素坐标。
    pub fn proc_image(
        &mut self,
        src_image: &impl IntoImageView,
        conf_th: f32,
        iou_th: f32,
    ) -> anyhow::Result<Vec<Detection>> {
        let _yolov5face = span!("YoloV5 Face");
        _yolov5face.emit_color(0x2f60fe);

        let input_array = {
            let _preproc = span!("Pre Proc");
            // letterbox（contain）：整图等比缩放进模型输入画布并居中，留边填充 114，
            // 保证任意宽高比的图片都不丢失边缘区域的人脸。
            let (sx, sy, ox, oy) =
                self.letterbox
                    .fill(src_image, &mut self.resizer, 114)?;
            let _ = (sx, sy, ox, oy);
            let canvas = &self.letterbox.canvas;
            let array_view = ArrayView::from_shape(
                (1, self.input_shape.0, self.input_shape.1, 3),
                canvas.buffer(),
            )?
            .permuted_axes([0, 3, 1, 2]); // [1, w, h, 3] -> [1, 3, w, h]

            let rgb_array_view = if self.rgb_input {
                array_view.slice(s![.., .., .., ..])
            } else {
                array_view.slice(s![.., ..;-1, .., ..])
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
            // [batch_size][4032][16{cxcywh:0..4, conf:4, landmarks:5..15, cls:15}]
            let _post_proc = span!("Post Proc");
            let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
            anyhow::ensure!(
                shape.len() == 3,
                "YoloV5Face 输出形状异常: {shape:?}"
            );
            let output = ArrayView3::from_shape(                (shape[0] as usize, shape[1] as usize, shape[2] as usize),
                data,
            )?;
            let output_batch = output.slice(s![0, .., ..]);

            let mut boxes: Vec<YoloBox> = Vec::new();
            for elem in output_batch.axis_iter(Axis(0)) {
                if let Some(e) = YoloBox::new(elem, conf_th, self.pos_scale) {
                    Self::nms_append(&mut boxes, e, iou_th);
                }
            }

            // 画布归一化坐标 -> 原图像素
            let src_w = src_image.width() as f32;
            let src_h = src_image.height() as f32;
            let (sx, sy, ox, oy) =
                crate::utils::letterbox_params((src_w as u32, src_h as u32), (self.input_shape.0 as u32, self.input_shape.1 as u32));
            let (inp_w, inp_h) = (self.input_shape.0 as f32, self.input_shape.1 as f32);
            return Ok(boxes
                .into_iter()
                .map(|b| Detection {
                    conf: b.conf,
                    bbox: Rect {
                        xy: ((b.bbox.xy.x * inp_w - ox) / sx,
                             (b.bbox.xy.y * inp_h - oy) / sy).into(),
                        size: (b.bbox.size.width * inp_w / sx,
                               b.bbox.size.height * inp_h / sy).into(),
                    },
                    landmarks: b
                        .landmarks
                        .iter()
                        .map(|p| ((p.0 * inp_w - ox) / sx, (p.1 * inp_h - oy) / sy))
                        .collect(),
                })
                .collect());
        }
    }

    #[inline]
    fn nms_append(results: &mut Vec<YoloBox>, result: YoloBox, iou_th: f32) {
        for r in results.iter_mut() {
            if result.iou(r) > iou_th {
                if result.conf > r.conf {
                    *r = result;
                }
                return;
            }
        }
        results.push(result);
    }
}
