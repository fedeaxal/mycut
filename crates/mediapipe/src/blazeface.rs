//! BlazeFace short-range 人脸检测器（mediapipe 官方 face_detection_short_range 模型）。
//!
//! 参数与解码公式均取自 mediapipe 官方源码，并经官方 python 实现（solutions API）
//! 输出交叉验证：
//! - anchor 生成: `ssd_anchors_calculator.cc`（strides 8/16/16/16, min/max scale
//!   0.1484375/0.75, interpolated_scale_aspect_ratio 1.0, fixed_anchor_size）
//! - 框解码: `tensors_to_detections_calculator.cc::DecodeBoxes` 的 **XYHW** 变体
//!   （raw/scale 128 * anchor 尺寸 + anchor 中心；score = sigmoid）。
//!   注意：该模型输出为 XYHW 而非默认 YXHW（已实测校准）。
//! - 模型图配置: `face_detection_short_range.pbtxt`（输入 128×128, num_boxes 896）
//!
//! 输入预处理与官方一致：整图 letterbox（contain，BORDER_ZERO），归一化到 [-1,1]。
//! 6 个关键点顺序（官方 solutions 文档）：右眼、左眼、鼻尖、嘴、右耳、左耳。

use fast_image_resize::{IntoImageView, Resizer};
use ndarray::{Array4, ArrayView, ArrayView2, Axis};
use ort::inputs;
use ort::value::TensorRef;
use rayon::iter::{ParallelBridge, ParallelIterator};
use std::path::Path;

use tracy_client::span;

use crate::geometry::Rect;
use crate::utils::{Detection, Ep, LetterboxCanvas, build_session};

/// 与官方 kShortRangeImageSize 一致
const INPUT_SIZE: u32 = 128;
const NUM_BOXES: usize = 896;
/// 每框坐标数：4 bbox (x,y,w,h) + 6 keypoints × 2（模型输出 16 维，见 doc 注释）
const KEYPOINT_OFFSET: usize = 4;
/// 官方 legacy 解码的 scale（kShortRangeImageSize）
const BOX_SCALE: f32 = 128.0;
/// anchor 配置（见模块注释）
const ANCHOR_STRIDES: [u32; 4] = [8, 16, 16, 16];
const ANCHOR_MIN_SCALE: f32 = 0.148_437_5;
const ANCHOR_MAX_SCALE: f32 = 0.75;

#[derive(Clone, Copy)]
struct Anchor {
    x_center: f32,
    y_center: f32,
    w: f32,
    h: f32,
}

#[inline]
fn calculate_scale(min_scale: f32, max_scale: f32, stride_index: usize, num_strides: usize) -> f32 {
    if num_strides == 1 {
        (min_scale + max_scale) * 0.5
    } else {
        min_scale + (max_scale - min_scale) * stride_index as f32 / (num_strides - 1) as f32
    }
}

/// 按官方 ssd_anchors_calculator.cc 的顺序生成 896 个 anchor。
/// fixed_anchor_size = true（tasks face_detector_graph.cc legacy 分支）→ w = h = 1。
fn generate_anchors() -> Vec<Anchor> {
    let mut anchors = Vec::with_capacity(NUM_BOXES);
    let num_layers = ANCHOR_STRIDES.len();
    let mut layer = 0;
    while layer < num_layers {
        // 同 stride 的层合并：每层贡献 2 个 anchor
        // (scale 及 sqrt(scale*scale_next)，aspect_ratio 均为 1.0)
        let mut scales_per_cell = 0usize;
        let mut last_same = layer;
        let mut interpolated = 0.0f32;
        while last_same < num_layers && ANCHOR_STRIDES[last_same] == ANCHOR_STRIDES[layer] {
            let sc = calculate_scale(ANCHOR_MIN_SCALE, ANCHOR_MAX_SCALE, last_same, num_layers);
            let sc_next = if last_same == num_layers - 1 {
                1.0
            } else {
                calculate_scale(ANCHOR_MIN_SCALE, ANCHOR_MAX_SCALE, last_same + 1, num_layers)
            };
            let _ = sc;
            interpolated = (sc * sc_next).sqrt();
            scales_per_cell += 2;
            last_same += 1;
        }
        let _ = interpolated; // fixed_anchor_size 下 scale 不影响 anchor 尺寸，仅数量
        let fm = (INPUT_SIZE as f32 / ANCHOR_STRIDES[layer] as f32).ceil() as u32;
        for y in 0..fm {
            for x in 0..fm {
                for _ in 0..scales_per_cell {
                    anchors.push(Anchor {
                        x_center: (x as f32 + 0.5) / fm as f32,
                        y_center: (y as f32 + 0.5) / fm as f32,
                        w: 1.0,
                        h: 1.0,
                    });
                }
            }
        }
        layer = last_same;
    }
    debug_assert_eq!(anchors.len(), NUM_BOXES);
    anchors
}

pub struct BlazeFace {
    session: ort::session::Session,
    resizer: Resizer,
    letterbox: LetterboxCanvas,
    anchors: Vec<Anchor>,
    input_array: Array4<f32>,
}

impl BlazeFace {
    pub fn new<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Self> {
        let session = build_session(model, ep, intra_threads)?;
        let dims = crate::utils::tensor_dims(session.inputs().first())?;
        anyhow::ensure!(
            dims[1] == INPUT_SIZE as i64 && dims[2] == INPUT_SIZE as i64,
            "BlazeFace 模型输入应为 {INPUT_SIZE}×{INPUT_SIZE}，实际为 {dims:?}"
        );
        Ok(Self {
            session,
            resizer: Resizer::new(),
            letterbox: LetterboxCanvas::new(INPUT_SIZE, INPUT_SIZE),
            anchors: generate_anchors(),
            input_array: unsafe {
                Array4::uninit((1, INPUT_SIZE as usize, INPUT_SIZE as usize, 3)).assume_init()
            },
        })
    }

    /// 检测人脸。conf_th 为 sigmoid 后的置信度阈值，iou_th 为 NMS IoU 阈值。
    pub fn proc_image(
        &mut self,
        src_image: &impl IntoImageView,
        conf_th: f32,
        iou_th: f32,
    ) -> anyhow::Result<Vec<Detection>> {
        let _bf = span!("BlazeFace");
        _bf.emit_color(0x2f60fe);

        // ---- 预处理：letterbox + 归一化到 [-1,1]（缓冲复用）----
        let (params, raw_img) = {
            let _pre = span!("Pre Proc");
            // BORDER_ZERO：填充黑（0 像素值），归一化后为 -1
            let params = self.letterbox.fill(src_image, &mut self.resizer, 0)?;
            let buf = self.letterbox.canvas.buffer();
            let view = ArrayView::from_shape((
                1,
                INPUT_SIZE as usize,
                INPUT_SIZE as usize,
                3,
            ), buf)?;
            let out = &mut self.input_array;
            out.axis_iter_mut(Axis(0))
                .zip(view.axis_iter(Axis(0)))
                .par_bridge()
                .for_each(|(mut new, old)| {
                    new.zip_mut_with(&old, |new, old| *new = (*old as f32 - 127.5) / 127.5);
                });
            (params, out.view())
        };

        // ---- 推理 ----
        let outputs = {
            let _inf = span!("Inference");
            self.session
                .run(inputs![TensorRef::from_array_view(raw_img)?])?
        };

        // ---- 解码（官方 XYHW 公式，经官方 python 实现校准）----
        let _post = span!("Post Proc");
        // 按形状识别输出（tf2onnx 的输出顺序不保证与 tflite 一致）
        let mut i_reg = None;
        let mut i_score = None;
        for (idx, (_, o)) in outputs.iter().enumerate() {
            let Ok((shape, _)) = o.try_extract_tensor::<f32>() else {
                continue;
            };
            let dims: Vec<i64> = shape.iter().copied().collect();
            if dims.len() == 3 && dims[1] == NUM_BOXES as i64 && dims[2] == 16 {
                i_reg = Some(idx);
            } else if dims.len() == 3 && dims[1] == NUM_BOXES as i64 && dims[2] == 1 {
                i_score = Some(idx);
            }
        }
        let (Some(i_reg), Some(i_score)) = (i_reg, i_score) else {
            anyhow::bail!("BlazeFace 输出形状异常: 无法识别 regressors/classificators");
        };
        let (_, reg_data) = outputs[i_reg].try_extract_tensor::<f32>()?;
        let (_, sc_data) = outputs[i_score].try_extract_tensor::<f32>()?;
        // 扁平数据 -> 静态二维视图，后续可用普通索引/row()
        let reg = ArrayView2::from_shape((NUM_BOXES, 16), reg_data)
            .map_err(|e| anyhow::anyhow!("BlazeFace regressors 形状不匹配: {e}"))?;
        let sc = ArrayView2::from_shape((NUM_BOXES, 1), sc_data)
            .map_err(|e| anyhow::anyhow!("BlazeFace classificators 形状不匹配: {e}"))?;

        let (sx, sy, ox, oy) = params;
        let src_w = src_image.width() as f32;
        let src_h = src_image.height() as f32;

        let mut results: Vec<Detection> = Vec::new();
        for (i, &anchor) in self.anchors.iter().enumerate() {
            let row = reg.row(i);
            // score = sigmoid(classificator)
            let score = crate::utils::sigmoid(&sc[[i, 0]]);
            if score <= conf_th {
                continue;
            }
            let y_center = row[1] / BOX_SCALE * anchor.h + anchor.y_center;
            let x_center = row[0] / BOX_SCALE * anchor.w + anchor.x_center;
            let h = row[3] / BOX_SCALE * anchor.h;
            let w = row[2] / BOX_SCALE * anchor.w;

            // 归一化画布坐标 -> 原图像素（letterbox 逆映射）
            let x0 = (x_center - w / 2.0) * INPUT_SIZE as f32;
            let y0 = (y_center - h / 2.0) * INPUT_SIZE as f32;
            let x1 = (x_center + w / 2.0) * INPUT_SIZE as f32;
            let y1 = (y_center + h / 2.0) * INPUT_SIZE as f32;
            let bbox = Rect {
                xy: ((x0 - ox) / sx, (y0 - oy) / sy).into(),
                size: ((x1 - x0) / sx, (y1 - y0) / sy).into(),
            };

            let mut landmarks = Vec::with_capacity(6);
            for k in 0..6 {
                let off = KEYPOINT_OFFSET + k * 2;
                let kx = row[off] / BOX_SCALE * anchor.w + anchor.x_center;
                let ky = row[off + 1] / BOX_SCALE * anchor.h + anchor.y_center;
                landmarks.push(((kx * INPUT_SIZE as f32 - ox) / sx, (ky * INPUT_SIZE as f32 - oy) / sy));
            }

            let (conf, bbox, mut lm) = (score, bbox, landmarks);
            // NMS（贪心：重叠则保留高分者）
            let mut keep = true;
            for r in results.iter_mut() {
                let inter = bbox.intersect(&r.bbox).area();
                if inter > f32::EPSILON {
                    let ov = inter / (bbox.area() + r.bbox.area() - inter);
                    if ov > iou_th {
                        keep = false;
                        if conf > r.conf {
                            r.bbox = bbox;
                            r.conf = conf;
                            std::mem::swap(&mut r.landmarks, &mut lm);
                        }
                        break;
                    }
                }
            }
            if keep {
                results.push(Detection {
                    conf,
                    bbox,
                    landmarks: lm,
                });
            }
        }
        let _ = (src_w, src_h);
        Ok(results)
    }
}
