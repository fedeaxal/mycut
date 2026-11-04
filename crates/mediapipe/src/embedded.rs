//! 编译期嵌入全部 onnx 模型（[`MODELS`]），使打包后的可执行文件不带
//! weights/ 目录也能在其他计算机上直接运行。
//!
//! 模型是二进制 protobuf（非 UTF-8 文本），只能用 include_bytes! 嵌入字节，
//! 不能用 include_str!。嵌入数据位于只读段，按页从磁盘懒加载——未实际
//! 加载会话的模型不占用运行内存，只有二进制体积成本。
//!
//! 磁盘文件优先：weights/ 下存在同名文件时照旧从文件加载（--weights-dir
//! 可热替换模型），文件缺失时才按文件名回落到嵌入副本。

/// (weights/ 下的文件名, 编译期嵌入的模型字节)。文件名是回落查找键，
/// 必须与传给 [`crate::utils::build_session`] 的路径尾名保持一致。
pub const MODELS: &[(&str, &[u8])] = &[
    (
        "blazeface_short_range.onnx",
        include_bytes!("../weights/blazeface_short_range.onnx"),
    ),
    (
        "yolov5n-face-relu.onnx",
        include_bytes!("../weights/yolov5n-face-relu.onnx"),
    ),
    (
        "face_landmarks_detector.onnx",
        include_bytes!("../weights/face_landmarks_detector.onnx"),
    ),
    (
        "face_blendshapes.onnx",
        include_bytes!("../weights/face_blendshapes.onnx"),
    ),
    (
        "selfie_segmenter.onnx",
        include_bytes!("../weights/selfie_segmenter.onnx"),
    ),
    (
        "selfie_multiclass_256x256.onnx",
        include_bytes!("../weights/selfie_multiclass_256x256.onnx"),
    ),
];

/// 按文件名查找编译期嵌入的模型字节。
pub fn model_bytes(file_name: &str) -> Option<&'static [u8]> {
    MODELS
        .iter()
        .find(|(name, _)| *name == file_name)
        .map(|(_, bytes)| *bytes)
}
