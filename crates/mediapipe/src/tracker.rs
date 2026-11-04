//! VIDEO 运行模式的帧间人脸跟踪，复刻 mediapipe face_landmarker_graph.cc 的
//! stream-mode 逻辑：
//! - 上一帧由 landmarks 推导的人脸矩形（scale 1.5 + square_long，即长边×1.5 的
//!   正方形）经 PREV_LOOP 回环作为下一帧 landmark 模型的 ROI；
//! - 跟踪数量足够（≥ num_faces）时跳过人脸检测器（官方 DisallowIf(has_enough_faces)）；
//! - 检测器重新启用时用 IoU 关联存活 track，避免重复计数（官方
//!   AssociationNormRectCalculator，阈值 min_tracking_confidence）。

use crate::geometry::Rect;

/// 官方 RectTransformationCalculator 的 scale_x/scale_y（face_landmarks_detector_graph.cc）
pub const ROI_SCALE: f32 = 1.5;
/// 官方 AssociationNormRectCalculator 的 min_similarity_threshold 默认值
pub const ASSOC_IOU_TH: f32 = 0.5;

/// 从 478 点推导检测器粗点（与 BlazeFace 6 点同语义）的索引：
/// [右眼虹膜中心, 左眼虹膜中心, 鼻尖, 嘴中心(上下唇中点), 右脸缘, 左脸缘]
pub const LANDMARKS_FROM_478: [(usize, usize); 6] =
    [(468, 0), (473, 0), (1, 0), (13, 14), (234, 0), (454, 0)];

#[derive(Default)]
pub struct FaceTracker {
    /// 下一帧的 landmark 模型 ROI（原图像素坐标），来自上一帧 landmarks
    pub next_rects: Vec<Rect>,
}

impl FaceTracker {
    pub fn new() -> Self {
        Self::default()
    }
}

/// 两矩形 IoU（用于检测框与存活 track 的关联去重）
pub fn iou(a: &Rect, b: &Rect) -> f32 {
    let i = a.intersect(b).area();
    if i < f32::EPSILON {
        return 0.0;
    }
    i / (a.area() + b.area() - i)
}

/// 将上一帧的 ROI 限制到当前图像范围内。
/// 跨帧图像尺寸变化（或浮点误差）时防止裁剪框越界；
/// 越界 track 被 clamp 后通常会因 landmarks 置信度过低而自然丢失并触发重检。
pub fn clamp_rect(r: Rect, width: u32, height: u32) -> Rect {
    let side = r
        .size
        .width
        .min(r.size.height)
        .min(width as f32)
        .min(height as f32)
        .max(1.0);
    // 减去 epsilon 避免 f32 累加误差使 left+side 恰好越界被 resize 库拒绝
    let max_left = (width as f32 - side - 1e-3).max(0.0);
    let max_top = (height as f32 - side - 1e-3).max(0.0);
    Rect {
        xy: (r.xy.x.clamp(0.0, max_left), r.xy.y.clamp(0.0, max_top)).into(),
        size: (side, side).into(),
    }
}

/// 由 landmarks 的外接框计算下一帧 ROI：长边 × 1.5 的正方形、中心保持、
/// 限制在图像范围内（官方 scale 1.5 + square_long + clip）。
pub fn roi_from_landmarks(
    points: &[(f32, f32, f32)],
    crop: Rect,
    width: u32,
    height: u32,
) -> Rect {
    // points 相对 crop 原点，先转图像坐标
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for p in points {
        let (x, y) = (p.0 + crop.xy.x, p.1 + crop.xy.y);
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    let side = f32::min(
        f32::max(max_x - min_x, max_y - min_y) * ROI_SCALE,
        f32::min(width as f32, height as f32),
    );
    let max_left = (width as f32 - side - 1e-3).max(0.0);
    let max_top = (height as f32 - side - 1e-3).max(0.0);
    Rect {
        xy: ((cx - side / 2.0).clamp(0.0, max_left), (cy - side / 2.0).clamp(0.0, max_top)).into(),
        size: (side, side).into(),
    }
}
