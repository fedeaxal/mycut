//! # facelandmark
//!
//! 基于 ONNX Runtime 的 MediaPipe 系列视觉推理库：
//! - 人脸检测（BlazeFace short-range / YoloV5Face，见 [`blazeface`]/[`yolov5_face`]）
//! - 478 点人脸关键点（MediaPipe Face Landmarks，见 [`face_landmark`]）
//! - 52 个表情系数 blendshapes（官方 face_blendshapes 模型，见 [`blendshapes`]）
//! - 人像分割（SelfieSegmenter，见 [`segmenter`]）
//! - VIDEO 模式帧间人脸跟踪（[`tracker`]）
//!
//! 所有模型的坐标输出均为输入图像的像素坐标；预处理/解码参数均取自 mediapipe
//! 官方实现并与官方 python 输出对齐。命令行工具见 `facelandmark-cli`。
//!
//! 全部模型编译期嵌入二进制（[`embedded`]）：weights/ 下的文件缺失时自动
//! 回落到嵌入副本，打包出的可执行文件可单独分发到其他计算机直接运行。
//!
//! `load-dynamic` 下 onnxruntime 动态库由使用方进程在首个会话之前加载——
//! `ort::init_from(路径)`。ort 从这里重导出，使用方不必再声明一份依赖。

pub mod blendshapes;
pub mod blazeface;
pub mod embedded;
pub mod face_landmark;
pub mod geometry;
pub mod segmenter;
pub mod tracker;
pub mod utils;
pub mod yolov5_face;

/// ONNX Runtime 绑定的重导出（load-dynamic 的 init_from 由使用方调用）。
pub use ort;
