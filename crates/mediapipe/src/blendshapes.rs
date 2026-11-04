//! MediaPipe FaceBlendshapes：由 478 点关键点推断 52 个表情系数。
//!
//! 使用官方 face_blendshapes 模型（ONNX 转换版，位于 weights/face_blendshapes.onnx，
//! 存在该文件时自动启用）。模型输入为 478 点中特定 146 个标记点的 (x, y)
//! 像素坐标（256×256 人脸裁剪空间），输出为 52 个已过 sigmoid 的表情系数。

use std::path::Path;

use ndarray::Array3;
use ort::inputs;
use ort::value::TensorRef;

use crate::utils::{Ep, build_session};

pub const BLENDSHAPES_NUM: usize = 52;
pub const SUBSET_NUM: usize = 146;
/// 模型输入的裁剪空间尺寸（face_landmarks_detector 输入为 256×256）
const CROP_SPACE: f32 = 256.0;

/// 官方模型所需的 478 点索引子集
/// （来源：mediapipe face_blendshapes_graph.cc 的 kLandmarksSubsetIdxs）
const LANDMARKS_SUBSET_IDXS: [usize; SUBSET_NUM] = [
    0, 1, 4, 5, 6, 7, 8, 10, 13, 14, 17, 21, 33, 37, 39, 40, 46, 52, 53, 54, 55, 58, 61, 63, 65,
    66, 67, 70, 78, 80, 81, 82, 84, 87, 88, 91, 93, 95, 103, 105, 107, 109, 127, 132, 133, 136,
    144, 145, 146, 148, 149, 150, 152, 153, 154, 155, 157, 158, 159, 160, 161, 162, 163, 168, 172,
    173, 176, 178, 181, 185, 191, 195, 197, 234, 246, 249, 251, 263, 267, 269, 270, 276, 282, 283,
    284, 285, 288, 291, 293, 295, 296, 297, 300, 308, 310, 311, 312, 314, 317, 318, 321, 323, 324,
    332, 334, 336, 338, 356, 361, 362, 365, 373, 374, 375, 377, 378, 379, 380, 381, 382, 384, 385,
    386, 387, 388, 389, 390, 397, 398, 400, 402, 405, 409, 415, 454, 466, 468, 469, 470, 471, 472,
    473, 474, 475, 476, 477,
];

/// 52 个表情系数名称，顺序与模型输出一一对应
/// （来源：mediapipe face_blendshapes_graph.cc 的 kBlendshapeNames）
pub const BLENDSHAPE_NAMES: [&str; BLENDSHAPES_NUM] = [
    "_neutral",
    "browDownLeft",
    "browDownRight",
    "browInnerUp",
    "browOuterUpLeft",
    "browOuterUpRight",
    "cheekPuff",
    "cheekSquintLeft",
    "cheekSquintRight",
    "eyeBlinkLeft",
    "eyeBlinkRight",
    "eyeLookDownLeft",
    "eyeLookDownRight",
    "eyeLookInLeft",
    "eyeLookInRight",
    "eyeLookOutLeft",
    "eyeLookOutRight",
    "eyeLookUpLeft",
    "eyeLookUpRight",
    "eyeSquintLeft",
    "eyeSquintRight",
    "eyeWideLeft",
    "eyeWideRight",
    "jawForward",
    "jawLeft",
    "jawOpen",
    "jawRight",
    "mouthClose",
    "mouthDimpleLeft",
    "mouthDimpleRight",
    "mouthFrownLeft",
    "mouthFrownRight",
    "mouthFunnel",
    "mouthLeft",
    "mouthLowerDownLeft",
    "mouthLowerDownRight",
    "mouthPressLeft",
    "mouthPressRight",
    "mouthPucker",
    "mouthRight",
    "mouthRollLower",
    "mouthRollUpper",
    "mouthShrugLower",
    "mouthShrugUpper",
    "mouthSmileLeft",
    "mouthSmileRight",
    "mouthStretchLeft",
    "mouthStretchRight",
    "mouthUpperUpLeft",
    "mouthUpperUpRight",
    "noseSneerLeft",
    "noseSneerRight",
];

pub struct FaceBlendshapes {
    session: ort::session::Session,
    /// 预分配的输入缓冲 [1, 146, 2]
    input_buf: Array3<f32>,
}

impl FaceBlendshapes {
    pub fn new<P: AsRef<Path>>(model: P, ep: Ep, intra_threads: usize) -> anyhow::Result<Self> {
        let session = build_session(model, ep, intra_threads)?;
        let dims = crate::utils::tensor_dims(session.inputs().first())?;
        anyhow::ensure!(
            dims[..] == [1, SUBSET_NUM as i64, 2],
            "blendshapes 模型输入形状应为 [1,{SUBSET_NUM},2]，实际为 {dims:?}（不是官方 face_blendshapes 模型？）"
        );
        Ok(Self {
            session,
            input_buf: Array3::zeros((1, SUBSET_NUM, 2)),
        })
    }

    /// points: 478 个 (x, y, z)，坐标已缩放到裁剪区域像素尺寸（scale_to 空间）；
    /// crop_size: 裁剪区域边长（原图像素，正方形）。
    /// 返回 52 个表情系数（已过 sigmoid，与 BLENDSHAPE_NAMES 一一对应）。
    pub fn proc(&mut self, points: &[(f32, f32, f32)], crop_size: f32) -> anyhow::Result<Vec<f32>> {
        let _bs = tracy_client::span!("Blendshapes");
        // 裁剪空间坐标 -> 256×256 模型输入空间
        let scale = CROP_SPACE / crop_size;
        for (i, &idx) in LANDMARKS_SUBSET_IDXS.iter().enumerate() {
            let p = points
                .get(idx)
                .ok_or_else(|| anyhow::anyhow!("关键点数量不足: 需要 {idx}，实际 {}", points.len()))?;
            self.input_buf[[0, i, 0]] = p.0 * scale;
            self.input_buf[[0, i, 1]] = p.1 * scale;
        }
        let outputs = self
            .session
            .run(inputs![TensorRef::from_array_view(self.input_buf.view())?])?;
        let (_, scores) = outputs[0]
            .try_extract_tensor::<f32>()
            .map_err(|e| anyhow::anyhow!("blendshapes 输出提取失败: {e}"))?;
        anyhow::ensure!(
            scores.len() == BLENDSHAPES_NUM,
            "blendshapes 输出数量 {} != {BLENDSHAPES_NUM}",
            scores.len()
        );
        Ok(scores.to_vec())
    }
}
