# facelandmark

基于 ONNX Runtime 的 MediaPipe 系列视觉推理 **Rust 库** 及其 **命令行工具** `facelandmark-cli`：

- **人脸检测**: BlazeFace short-range（默认，官方同款 128×128）/ YoloV5Face（备选，640×640）
- **人脸关键点**: MediaPipe Face Landmarker 478 点
- **表情系数**: MediaPipe face_blendshapes 52 个 ARKit 风格系数
- **人像分割**: MediaPipe SelfieSegmenter（256×256 alpha 掩码）

跨平台（Windows / macOS / Linux），纯 CPU 下即达实时（VIDEO 跟踪模式 56~72fps @ 4 核 i3）。

## 作为库使用

```toml
[dependencies]
facelandmark = "0.2"
```

```rust
use fast_image_resize::{PixelType, images::ImageRef};
use facelandmark::blazeface::BlazeFace;
use facelandmark::segmenter::SelfieSegmenter;
use facelandmark::utils::Ep;

let img = image::open("photo.jpg")?;
let (w, h) = (img.width(), img.height());
let rgb = img.into_rgb8();
let src = ImageRef::new(w, h, rgb.as_raw(), PixelType::U8x3)?;

// 人脸检测（坐标为原图像素）
let mut detector = BlazeFace::new("weights/blazeface_short_range.onnx", Ep::Auto, 2)?;
for d in detector.proc_image(&src, 0.6, 0.5)? {
    println!("face conf={:.3} bbox={:?}", d.conf, d.bbox);
}

// 人像分割（256×256 alpha 掩码，0~1）
let mut segmenter = SelfieSegmenter::new("weights/selfie_segmenter.onnx", Ep::Auto, 2)?;
let mask = segmenter.proc_image(&src)?;
```

完整示例见 [`examples/faces_demo.rs`](examples/faces_demo.rs)（`cargo run --example faces_demo -- photo.jpg`）。公共模块：

| 模块 | 内容 |
|---|---|
| `blazeface` / `yolov5_face` | 人脸检测器（`Detection`：bbox + 置信度 + 粗定位点，原图像素坐标） |
| `face_landmark` | 478 点关键点（`FaceLandmarkResult`：points/score/tongue） |
| `blendshapes` | 52 表情系数（输入 478 点 + 裁剪尺寸） |
| `segmenter` | SelfieSegmenter 人像分割（`SegmentationMask`） |
| `tracker` | VIDEO 模式帧间跟踪（ROI 推导 + 检测门控语义） |
| `utils` | `Ep` 执行提供者选择、`Detection`、`letterbox_params`、会话构建 |

## 命令行工具

```bash
cargo build --release
# 产物: target/release/facelandmark-cli，库: target/release/libfacelandmark.rlib

facelandmark-cli photo.jpg                       # 人脸检测 + 关键点 + blendshapes
facelandmark-cli --ort-lib /usr/local/lib/libonnxruntime.dylib photo.jpg
ls frames/*.jpg | facelandmark-cli --stream --running-mode video > results.jsonl
facelandmark-cli --no-landmarks photo.jpg        # 只要检测框（最快）
facelandmark-cli --segment photo.jpg             # 人像分割 -> photo_mask.png + JSON
facelandmark-cli --segment --segmenter multiclass photo.jpg  # 更高质量（较慢）
```

依赖动态链接的 ONNX Runtime（`ort` crate 的 `load-dynamic` 模式）：

- **Windows**: `pip install onnxruntime-directml==1.20.0 && python copy_runtime.py`（拷贝到 `target/{debug,release}`；auto 模式走 DirectML GPU，不可用回退 CPU）
- **macOS**: `brew install onnxruntime`，若 dlopen 找不到库则加 `--ort-lib $(brew --prefix)/lib/libonnxruntime.dylib`
- **Linux**: 安装 onnxruntime 后放到系统库路径，或用 `--ort-lib` 指定 `libonnxruntime.so`

也可用环境变量 `ORT_DYLIB_PATH` 指定库路径（效果同 `--ort-lib`）。

## 命令行选项

| 选项 | 默认 | 说明 |
|---|---|---|
| `--segment` | 关 | 人像分割模式（替代人脸管线），掩码存 PNG |
| `--segmenter <S>` | `selfie` | 分割模型：`selfie`（最快）\|`multiclass`（质量更高） |
| `--output <PATH>` | `<名>_mask.png` | 分割掩码输出路径（stream 下自动编号） |
| `--conf <F>` | `0.6` | 检测置信度阈值 |
| `--detector <D>` | `blazeface` | 检测器：`blazeface`\|`yolov5` |
| `--iou <F>` | `0.5` | NMS IoU 阈值 |
| `--running-mode <M>` | `image` | `image`\|`video`，语义同 mediapipe（见下） |
| `--num-faces <N>` | `0`（不限） | 最多输出人脸数；video 模式兼作官方 num_faces 语义 |
| `--no-landmarks` | 关 | 跳过 478 点关键点检测 |
| `--no-blendshapes` | 关 | 跳过 52 个表情系数 |
| `--weights-dir <DIR>` | `./weights` | 模型目录 |
| `--ort-lib <PATH>` | 自动 | onnxruntime 动态库路径 |
| `--ep <P>` | `auto` | 执行提供者：`auto`\|`coreml`\|`directml`\|`cpu` |
| `--threads <N>` | 核心数-2 | CPU 推理线程数 |
| `--stream` | 关 | 流式模式（stdin 每行一个路径，模型只加载一次） |
| `--pretty` | 关 | 美化 JSON 输出 |
| `--tracy` | 关 | 启动 Tracy Profiler 采集 |
| `-h, --help` | | 帮助 |

### 运行模式（--running-mode）

与 mediapipe FaceLandmarker 的 `running_mode` 语义一致（实现参照官方 `face_landmarker_graph.cc`）：

- **image**：每帧对整图运行人脸检测（默认，无时序假设）。
- **video**：帧间跟踪——上一帧由 478 点 landmarks 推导人脸 ROI（官方参数 scale 1.5 + square_long），下一帧直接在该 ROI 上跑关键点模型，**跳过整图人脸检测器**；跟踪数量达到 `--num-faces` 或有跟踪丢失时才重新检测，检测框与存活 track 用 IoU 关联去重（官方 `AssociationNormRectCalculator` 语义）。适合 `--stream` 喂连续视频帧；`--no-landmarks` 时无法跟踪，自动按 image 模式处理。

### 性能说明

- 默认执行提供者：Windows=DirectML，macOS/Linux=CPU。**Apple Silicon 建议 `--ep coreml`**；Intel Mac 核显实测 CoreML 远慢于 CPU（默认已避开）。
- 默认线程数为逻辑核心数-2：推理期间主线程还要解码/前后处理，占满核心会因争抢产生抖动（4 核实测：4 线程 47~185ms 抖动，2 线程稳定）。
- `--stream` 流式模式为连续视频帧设计：模型只加载一次，预处理缓冲全部预分配复用（逐帧零堆分配）。
- 4 核 i3-8100 实测（单脸、含解码）：BlazeFace image 模式 ~24ms/帧（41fps）；video 跟踪模式 ~15ms/帧（65fps，区间 14~17.5）；`--no-blendshapes` ~14ms（72fps）。分割：selfie ~10-12ms/张，multiclass ~130-150ms/张（质量更高）。
- 单次 30 帧 bench 在 4 核机器上有 ±20% 采样方差，报告性能请用多轮中位数。

## 输出格式（stdout, JSON）

人脸模式：

```jsonc
{
  "image": "photo.jpg",
  "width": 1080, "height": 2356,
  "num_faces": 1,
  "faces": [
    {
      "confidence": 0.92,              // 人脸置信度
      "bbox": [x, y, w, h],            // 人脸框，原图像素坐标
      "landmarks": [[x,y] ...],        // 检测器粗定位点（blazeface 6 点 / yolov5 5 点）
      "landmarks478": {                // MediaPipe 478 点（--no-landmarks 时无此字段）
        "score": 0.99,                 // 关键点置信度（<0.5 时整个对象为 null）
        "tongue": 0.02,
        "points": [[x, y, z] x 478],   // x/y 为原图像素坐标, z 为归一化深度
        "blendshapes": [               // 52 个表情系数（需 face_blendshapes.onnx）
          {"name": "mouthSmileLeft", "score": 0.79}, ...
        ]
      }
    }
  ],
  "timings_ms": { "decode": 14.4, "detect": 16.8, "landmarks478": 13.4, "blendshapes": 2.2 }
}
```

分割模式（`--segment`）：

```jsonc
{
  "image": "photo.jpg",
  "width": 1080, "height": 2356,
  "mask": "photo_mask.png",            // 原图尺寸灰度掩码（人像=白，背景=黑）
  "timings_ms": { "decode": 12.7, "segment": 12.2 }
}
```

检测采用 letterbox（整图等比缩放进模型输入画布），输出坐标已全部映射回原图像素。流式模式下单行失败输出 `{"image":..., "error":...}` 并继续。

## 推理模型（weights/，编译期嵌入）

| 文件 | 来源 | 说明 |
|---|---|---|
| `blazeface_short_range.onnx` | mediapipe 官方 `face_detection_short_range.tflite` 经 tf2onnx | 128×128，896 anchors；解码/anchor 按官方源码实现并与官方 python 输出对齐。6 粗点：右眼/左眼/鼻尖/嘴/右耳/左耳。近景模型：画布内人脸 <15px 左右低于检测极限 |
| `yolov5n-face-relu.onnx` | [YoloV5 Face](https://github.com/rede97?tab=repositories) | 640×640，远景小脸更强。`--detector yolov5`。5 粗点：左眼/右眼/鼻尖/两嘴角 |
| `face_landmarks_detector.onnx` | MediaPipe Face Landmarker | 478 点，TFLite → ONNX |
| `face_blendshapes.onnx` | mediapipe 官方 face_blendshapes 模型 ONNX 转换版 | 52 系数；输入官方 146 标记点子集（`face_blendshapes_graph.cc`）×(x,y)，256×256 裁剪空间坐标。存在即自动启用 |
| `selfie_segmenter.onnx` | mediapipe 官方 selfie_segmenter（square 256）ONNX 转换版 | 官方 tflite 含专有算子 `Convolution2DTransposeBias` 无法直接转 ONNX，采用社区转换版（onnx-community/mediapipe_selfie_segmentation，预处理与官方一致：256×256 双线性 + /255）。默认分割模型，462KB，~10ms |
| `selfie_multiclass_256x256.onnx` | mediapipe 官方 selfie_multiclass_256x256 转换版 | 6 类逐像素分割（头发/皮肤/衣物等）合并为人像掩码，边缘/发丝质量明显更好；16MB，CPU ~130-150ms，`--segmenter multiclass` 启用 |

上表全部模型在编译期以 `include_bytes!` 嵌入二进制（见 `src/embedded.rs`）：可执行文件
单独分发到其他计算机（不带 `weights/` 目录）时自动使用内置副本；`weights/` 下存在同名
文件时优先用文件，可用 `--weights-dir` 指定其他目录热替换模型，无需重新编译。

## 性能分析

保留原工程的 Tracy 集成：`--tracy` 启动采集，用 [Tracy Profiler](https://github.com/wolfpld/tracy) 连接后可查看每阶段（预处理/推理/后处理/blendshapes/分割）耗时。

## 原相机 GUI demo

原实时光机 GUI demo 的源码保留在 [`gui-demo-backup/`](gui-demo-backup/)。注意：CLI 版将检测预处理从「中心正方形裁剪」改为了 letterbox、输入通道从 BGRA(U8x4) 改为 RGB(U8x3)，恢复 GUI 时需要同步适配检测器的坐标假设。
