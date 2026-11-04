//! facelandmark-cli: 输入一张图片，输出其中人脸的 bounding box、置信度与关键点，
//! 或运行人像分割（--segment）。
//!
//! 基于 `facelandmark` 库（见 src/lib.rs）：检测 (BlazeFace/YoloV5Face)、
//! 关键点 (MediaPipe 478 点)、表情系数 (52 blendshapes)、人像分割 (SelfieSegmenter)。
//!
//! `--stream` 模式从 stdin 逐行读取图片路径（连续视频帧场景），模型只加载一次，
//! 单帧总耗时 = 解码 + 推理，无逐帧初始化开销。

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Error, bail};
use fast_image_resize::{PixelType, images::ImageRef};

use facelandmark::blendshapes::FaceBlendshapes;
use facelandmark::embedded;
use facelandmark::face_landmark::FaceLandmark;
use facelandmark::geometry::Rect;
use facelandmark::segmenter::{SegmenterKind, SelfieSegmenter};
use facelandmark::tracker::FaceTracker;
use facelandmark::utils::{self, Detection, Ep};
use facelandmark::{blazeface, blendshapes, face_landmark, measure_time, tracker, yolov5_face};

const DETECT_MODEL_FILE: &str = "blazeface_short_range.onnx";
const YOLOV5_MODEL_FILE: &str = "yolov5n-face-relu.onnx";
const LANDMARK_MODEL_FILE: &str = "face_landmarks_detector.onnx";
const BLENDSHAPES_MODEL_FILE: &str = "face_blendshapes.onnx";
/// 关键点裁剪区域相对 bbox 长边的外扩倍数，与原 demo / 官方 ROI scale 一致
const LANDMARK_CROP_MARGIN: f32 = 1.5;

/// 检测器选择。默认 blazeface（官方 mediapipe 同款，128×128 输入，更快）。
#[derive(Clone, Copy, Debug, PartialEq)]
enum DetectorKind {
    BlazeFace,
    YoloV5,
}

impl DetectorKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "blazeface" => Some(Self::BlazeFace),
            "yolov5" => Some(Self::YoloV5),
            _ => None,
        }
    }
}

/// 运行模式，语义与 mediapipe FaceLandmarker 的 running_mode 一致：
/// IMAGE = 每帧全图检测；VIDEO = 用上一帧 landmarks 推导 ROI 跟踪，
/// 跟踪数量足够时跳过人脸检测器（大幅提速），跟踪丢失时自动重新检测。
#[derive(Clone, Copy, Debug, PartialEq)]
enum RunningMode {
    Image,
    Video,
}

/// 检测器句柄：两种实现输出统一（原图像素坐标）
enum Detector {
    BlazeFace(blazeface::BlazeFace),
    YoloV5(yolov5_face::YoloV5Face),
}

impl Detector {
    fn proc_image(
        &mut self,
        img: &ImageRef,
        conf_th: f32,
        iou_th: f32,
    ) -> anyhow::Result<Vec<Detection>> {
        match self {
            Detector::BlazeFace(d) => d.proc_image(img, conf_th, iou_th),
            Detector::YoloV5(d) => d.proc_image(img, conf_th, iou_th),
        }
    }
}

/// 分割器句柄
enum Segmenter {
    Selfie(SelfieSegmenter),
    Multiclass(facelandmark::segmenter::MulticlassSegmenter),
}

impl Segmenter {
    fn proc_image(&mut self, img: &ImageRef) -> anyhow::Result<facelandmark::segmenter::SegmentationMask> {
        match self {
            Segmenter::Selfie(s) => s.proc_image(img),
            Segmenter::Multiclass(s) => s.proc_image(img),
        }
    }
}

struct Args {
    image_path: PathBuf,
    conf_th: f32,
    iou_th: f32,
    detector: DetectorKind,
    run_landmarks: bool,
    run_blendshapes: bool,
    /// 人像分割模式（替代人脸检测管线）
    segment: bool,
    /// 分割模型选择
    segmenter: SegmenterKind,
    /// 分割模式输出的掩码 PNG 路径（默认 <图片名>_mask.png；stream 模式自动加序号）
    output: Option<PathBuf>,
    pretty: bool,
    tracy: bool,
    stream: bool,
    running_mode: RunningMode,
    num_faces: usize,
    ep: Ep,
    threads: usize,
    ort_lib: Option<PathBuf>,
    weights_dir: PathBuf,
}

fn print_help() {
    println!(
        "facelandmark-cli - 人脸检测与关键点提取命令行工具

用法:
  facelandmark-cli [选项] <图片路径>
  facelandmark-cli [选项] --stream < 图片路径列表   # 每行一个路径，模型只加载一次
  facelandmark-cli --segment [选项] <图片路径>      # 人像分割模式

选项:
  --segment            人像分割模式: 输出人像 alpha 掩码，掩码 PNG 保存到
                       --output 路径，JSON 打印到 stdout
  --segmenter <S>      分割模型 selfie|multiclass (默认 selfie: 462KB 最快;
                       multiclass: 官方 6 类模型 16MB, 人像边缘质量更高)
  --output <路径>      分割掩码输出路径 (默认 <图片名>_mask.png;
                       --stream 下自动追加帧序号)
  --conf <阈值>        检测置信度阈值 (默认 0.6)
  --iou <阈值>         NMS IoU 阈值 (默认 0.5)
  --detector <D>       人脸检测器 blazeface|yolov5 (默认 blazeface:
                       mediapipe 官方同款 128×128, 更快更轻; yolov5 为
                       原仓库模型 640×640)
  --running-mode <M>   运行模式 image|video，语义同 mediapipe (默认 image)。
                       video: 用上一帧 landmarks 推导 ROI 跟踪，跟踪数量足够时
                       跳过人脸检测器（约 2 倍提速），丢失时自动重新检测；
                       需配合 --stream 使用，且不能加 --no-landmarks
  --num-faces <N>      最多输出的人脸数 (默认 0 = 不限)。
                       video 模式下兼作官方 num_faces 语义：跟踪数达到 N 后
                       停用检测器；默认 0 时检测器只在有跟踪丢失后才重启用
  --no-landmarks       跳过 478 点关键点检测（仅输出 bbox/置信度/5 点）
  --no-blendshapes     跳过 52 个表情系数（模型文件或内置嵌入副本存在时自动启用）
  --weights-dir <目录> 模型目录 (默认 ./weights)。所有模型已编译期嵌入本程序，
                       目录/文件缺失时自动使用内置副本，文件存在时优先用文件
  --ort-lib <路径>     onnxruntime 动态库路径 (默认从系统路径查找)
  --ep <provider>      执行提供者: auto|coreml|directml|cpu (默认 auto:
                       Windows=DirectML, 其余平台=CPU; Apple Silicon 建议
                       --ep coreml, Intel Mac 核显实测 CoreML 远慢于 CPU)
  --threads <N>        CPU 推理线程数 (默认 = 逻辑核心数 - 2)
  --stream             流式模式: 从 stdin 逐行读取图片路径，复用已加载的模型，
                       适用于连续视频帧；每行输出一行紧凑 JSON（--pretty 失效）
  --pretty             美化 JSON 输出
  --tracy              启动 Tracy 采集 (配合 Tracy Profiler 做性能分析)
  -h, --help           显示本帮助

输出 (stdout, 单行 JSON):
  {{ \"image\", \"width\", \"height\", \"num_faces\",
     \"faces\": [ {{ \"confidence\", \"bbox\": [x,y,w,h],
                    \"landmarks\": [[x,y] ...],   # 检测器粗定位点
                    \"landmarks478\": {{ \"score\", \"tongue\", \"points\": [[x,y,z] x 478],
                                       \"blendshapes\": [{{\"name\",\"score\"}} x 52] }} }} ],
     \"timings_ms\": {{ \"decode\", \"detect\", \"landmarks478\", \"blendshapes\" }} }}
  坐标均为原图像素坐标; landmarks478 的 z 为 MediaPipe 归一化深度。
  landmarks 为检测器粗点: blazeface 6 点(右眼/左眼/鼻尖/嘴/右耳/左耳),
  yolov5 5 点(左眼/右眼/鼻尖/两嘴角); VIDEO 跟踪帧由 478 点同语义推导。
  weights/ 下存在 face_blendshapes.onnx（或内置嵌入副本）时自动输出 52 个表情系数。
  无 landmarks478 需求时加 --no-landmarks 可显著提速。"
    );
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        image_path: PathBuf::new(),
        conf_th: 0.6,
        iou_th: 0.5,
        detector: DetectorKind::BlazeFace,
        run_landmarks: true,
        run_blendshapes: true,
        segment: false,
        segmenter: SegmenterKind::Selfie,
        output: None,
        pretty: false,
        tracy: false,
        stream: false,
        running_mode: RunningMode::Image,
        num_faces: 0,
        ep: Ep::Auto,
        threads: utils::default_threads(),
        ort_lib: None,
        weights_dir: PathBuf::from("weights"),
    };
    let mut has_image = false;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        // 非选项开头的一律视为图片路径（不拆分 '='，避免路径中含 '=' 被误解析）
        if !arg.starts_with('-') {
            if has_image {
                return Err(format!("只能输入一个图片路径，多余的参数: {arg}"));
            }
            args.image_path = arg.into();
            has_image = true;
            continue;
        }

        let (key, inline_val) = match arg.split_once('=') {
            Some((k, v)) => (k.to_owned(), Some(v.to_owned())),
            None => (arg, None),
        };
        let mut take_val = || {
            inline_val
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("选项 {key} 缺少值"))
        };

        match &*key {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--conf" => {
                args.conf_th = take_val()?
                    .parse()
                    .map_err(|_| format!("--conf 需要一个 (0,1] 内的浮点数"))?;
                if !(0.0..=1.0).contains(&args.conf_th) || args.conf_th <= 0.0 {
                    return Err(format!("--conf 需要一个 (0,1] 内的浮点数"));
                }
            }
            "--iou" => {
                args.iou_th = take_val()?
                    .parse()
                    .map_err(|_| "--iou 需要一个 (0,1] 内的浮点数".to_owned())?;
                if !(0.0..=1.0).contains(&args.iou_th) || args.iou_th <= 0.0 {
                    return Err("--iou 需要一个 (0,1] 内的浮点数".to_owned());
                }
            }
            "--detector" => {
                args.detector = DetectorKind::parse(&take_val()?)
                    .ok_or_else(|| "--detector 只支持 blazeface|yolov5".to_owned())?;
            }
            "--no-landmarks" => args.run_landmarks = false,
            "--no-blendshapes" => args.run_blendshapes = false,
            "--segment" => args.segment = true,
            "--segmenter" => {
                args.segmenter = SegmenterKind::parse(&take_val()?)
                    .ok_or_else(|| "--segmenter 只支持 selfie|multiclass".to_owned())?;
            }
            "--output" => args.output = Some(PathBuf::from(take_val()?)),
            "--running-mode" => {
                args.running_mode = match take_val()?.to_ascii_lowercase().as_str() {
                    "image" => RunningMode::Image,
                    "video" => RunningMode::Video,
                    _ => return Err("--running-mode 只支持 image|video".to_owned()),
                };
            }
            "--num-faces" => {
                args.num_faces = take_val()?
                    .parse()
                    .map_err(|_| "--num-faces 需要一个非负整数".to_owned())?;
            }
            "--weights-dir" => args.weights_dir = PathBuf::from(take_val()?),
            "--ort-lib" => args.ort_lib = Some(PathBuf::from(take_val()?)),
            "--ep" => {
                args.ep = Ep::parse(&take_val()?)
                    .ok_or_else(|| "--ep 只支持 auto|coreml|directml|cpu".to_owned())?;
            }
            "--threads" => {
                args.threads = take_val()?
                    .parse()
                    .map_err(|_| "--threads 需要一个正整数".to_owned())?;
                if args.threads == 0 {
                    return Err("--threads 需要一个正整数".to_owned());
                }
            }
            "--stream" => args.stream = true,
            "--pretty" => args.pretty = true,
            "--tracy" => args.tracy = true,
            _ => return Err(format!("未知选项: {key}（-h 查看帮助）")),
        }
    }

    if !args.stream && !has_image {
        return Err("缺少图片路径参数（-h 查看帮助）".into());
    }
    Ok(args)
}

/// 由人脸 bbox（原图像素坐标）计算关键点模型的正方形裁剪区域（原图像素坐标），
/// 长边按 margin 外扩并限制在图像范围内。逻辑与原 demo 一致。
#[inline]
fn landmark_crop_from_bbox(bbox: Rect, img_size: (f32, f32), margin: f32) -> Rect {
    let max_size = f32::max(bbox.size.width, bbox.size.height) * margin;
    let max_size = f32::min(max_size, f32::min(img_size.0, img_size.1));
    let half_max_size = max_size / 2.0;
    let center = (
        bbox.xy.x + bbox.size.width / 2.0,
        bbox.xy.y + bbox.size.height / 2.0,
    );

    let mut left = f32::max(0.0, center.0 - half_max_size);
    if left + max_size > img_size.0 {
        left = (img_size.0 - max_size - 1e-3).max(0.0);
    }
    let mut top = f32::max(0.0, center.1 - half_max_size);
    if top + max_size > img_size.1 {
        top = (img_size.1 - max_size - 1e-3).max(0.0);
    }

    Rect {
        xy: (left, top).into(),
        size: (max_size, max_size).into(),
    }
}

/// 仅对字符串值做 JSON 转义（图片路径可能含引号/反斜杠/控制字符）
fn escape_json(s: &str, out: &mut impl Write) -> std::io::Result<()> {
    for c in s.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => {
                let mut buf = [0u8; 4];
                out.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    Ok(())
}

/// 一帧中的人脸输出（坐标均为原图像素坐标）
struct FaceOut {
    /// 人脸框：IMAGE 模式来自检测器，VIDEO 跟踪帧来自 478 点外接框
    bbox: Rect,
    conf: f32,
    /// landmark 模型的裁剪区域
    crop: Rect,
    /// 检测器粗定位点（跟踪帧由 478 点同语义推导）
    landmarks: Vec<(f32, f32)>,
    lmk: Option<face_landmark::FaceLandmarkResult>,
}

/// 跟踪帧的粗定位点：从 478 点按检测器语义推导（右眼/左眼/鼻尖/嘴/右耳/左耳）
fn landmarks_from_478(points: &[(f32, f32, f32)], crop: Rect) -> Vec<(f32, f32)> {
    tracker::LANDMARKS_FROM_478
        .iter()
        .map(|&(i, j)| {
            let px = if j == 0 {
                points[i].0
            } else {
                (points[i].0 + points[j].0) / 2.0
            };
            let py = if j == 0 {
                points[i].1
            } else {
                (points[i].1 + points[j].1) / 2.0
            };
            (crop.xy.x + px, crop.xy.y + py)
        })
        .collect()
}

/// 478 点的外接框（原图像素坐标）
fn points_bbox(points: &[(f32, f32, f32)], crop: Rect) -> Rect {
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
    Rect {
        xy: (min_x, min_y).into(),
        size: (max_x - min_x, max_y - min_y).into(),
    }
}

/// 分割模式的掩码输出路径：<图片名>_mask.png（stream 模式带帧序号）。
fn mask_output_path(input: &Path, base: Option<&Path>, frame_no: usize) -> PathBuf {
    if let Some(p) = base {
        if frame_no == 0 {
            return p.to_path_buf();
        }
        let stem = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mask".into());
        let ext = p.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_else(|| "png".into());
        return p.with_file_name(format!("{stem}_{frame_no:03}.{ext}"));
    }
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    let mut p = input.to_path_buf();
    p.set_file_name(if frame_no == 0 {
        format!("{stem}_mask.png")
    } else {
        format!("{stem}_mask_{frame_no:03}.png")
    });
    p
}

/// 分割一张图：保存原图尺寸的灰度掩码 PNG，JSON 写入 buf。
fn process_segment_image(
    path: &Path,
    args: &Args,
    segmenter: &mut Segmenter,
    frame_no: usize,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let (img, decode_dur) = measure_time!({
        image::open(path).with_context(|| format!("读取图片失败: {}", path.display()))?
    });
    let (width, height) = (img.width(), img.height());
    if width == 0 || height == 0 {
        bail!("图片尺寸为空");
    }
    let rgb = img.into_rgb8();
    let src_img = ImageRef::new(width, height, rgb.as_raw(), PixelType::U8x3)
        .map_err(|e| Error::msg(e.to_string()))?;

    let (mask, seg_dur) = measure_time!({
        segmenter
            .proc_image(&src_img)
            .context("人像分割推理失败")?
    });

    let mask_path = mask_output_path(path, args.output.as_deref(), frame_no);
    let gray = mask.to_gray_resized(width, height);
    gray.save(&mask_path)
        .with_context(|| format!("保存掩码失败: {}", mask_path.display()))?;

    let pretty = args.pretty;
    let l0 = if pretty { "\n  " } else { "" };
    buf.write_all(b"{")?;
    buf.write_all(l0.as_bytes())?;
    buf.write_all(b"\"image\":\"")?;
    escape_json(&path.display().to_string(), buf)?;
    buf.write_all(b"\",")?;
    buf.write_all(l0.as_bytes())?;
    write!(buf, "\"width\":{width},")?;
    buf.write_all(l0.as_bytes())?;
    write!(buf, "\"height\":{height},")?;
    buf.write_all(l0.as_bytes())?;
    buf.write_all(b"\"mask\":\"")?;
    escape_json(&mask_path.display().to_string(), buf)?;
    buf.write_all(b"\",")?;
    buf.write_all(l0.as_bytes())?;
    write!(
        buf,
        "\"timings_ms\":{{\"decode\":{:.2},\"segment\":{:.2}}}",
        decode_dur.as_secs_f32() * 1000.0,
        seg_dur.as_secs_f32() * 1000.0
    )?;
    if pretty {
        buf.write_all(b"\n")?;
    }
    buf.write_all(b"}")?;
    Ok(())
}

/// 分割单图模式
fn run_segment_single(args: &Args, segmenter: &mut Segmenter) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(16 * 1024, stdout.lock());
    let mut buf = Vec::with_capacity(4 * 1024);
    process_segment_image(&args.image_path, args, segmenter, 0, &mut buf)?;
    out.write_all(&buf)?;
    out.write_all(b"\n")?;
    out.flush().context("写出结果失败")?;
    Ok(())
}

/// 分割流式模式：每行一个图片路径，掩码自动编号保存
fn run_segment_stream(args: &Args, segmenter: &mut Segmenter) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(16 * 1024, stdout.lock());
    let mut buf = Vec::with_capacity(4 * 1024);
    let stdin = std::io::stdin();
    let mut frame_no = 0usize;
    for line in std::io::BufRead::lines(stdin.lock()) {
        let line = line.context("读取 stdin 失败")?;
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        buf.clear();
        match process_segment_image(Path::new(path), args, segmenter, frame_no, &mut buf) {
            Ok(()) => {
                frame_no += 1;
                out.write_all(&buf)?;
                out.write_all(b"\n")?;
            }
            Err(e) => {
                out.write_all(b"{\"image\":\"")?;
                escape_json(path, &mut out)?;
                out.write_all(b"\",\"error\":\"")?;
                escape_json(&format!("{e:#}"), &mut out)?;
                out.write_all(b"\"}\n")?;
            }
        }
        out.flush()?;
    }
    Ok(())
}

/// 推理一张图并把结果 JSON 写入 buf（单行；pretty 时多行）。
/// 先写进调用方复用的缓冲再统一落盘，保证流式模式下不会出现残缺行。
#[allow(clippy::too_many_arguments)]
fn process_image(
    path: &Path,
    args: &Args,
    detector: &mut Detector,
    landmarker: &mut Option<FaceLandmark>,
    blendshapes: &mut Option<FaceBlendshapes>,
    tracker: &mut FaceTracker,
    buf: &mut Vec<u8>,
) -> Result<(), Error> {
    let (img, decode_dur) = measure_time!({
        image::open(path).with_context(|| format!("读取图片失败: {}", path.display()))?
    });
    let (width, height) = (img.width(), img.height());
    if width == 0 || height == 0 {
        bail!("图片尺寸为空");
    }
    // 解码为 RGB8：JPEG 等常见格式解码本身即为 RGB，此处零拷贝
    let rgb = img.into_rgb8();
    let src_img = ImageRef::new(width, height, rgb.as_raw(), PixelType::U8x3)
        .map_err(|e| Error::msg(e.to_string()))?;

    let img_size = (width as f32, height as f32);

    let mut detect_dur = Duration::ZERO;
    let mut lmk_total = Duration::ZERO;
    let mut bs_total = Duration::ZERO;
    let mut faces: Vec<FaceOut> = Vec::new();
    // 下一帧的跟踪 ROI（VIDEO 模式）；IMAGE 模式始终为空
    let mut next_rects: Vec<Rect> = Vec::new();

    // VIDEO 模式：用上一帧 landmarks 推导的 ROI 跟踪，跳过检测器。
    // 与官方一致（face_landmarker_graph.cc stream-mode：PREV_LOOP 回环 +
    // DisallowIf(has_enough_faces) 门控检测器）。--no-landmarks 时无法跟踪，退回 IMAGE 行为。
    let video_mode = args.running_mode == RunningMode::Video && landmarker.is_some();
    let prev_rects = if video_mode {
        std::mem::take(&mut tracker.next_rects)
    } else {
        Vec::new()
    };

    let mut run_landmark =
        |crop: Rect| -> anyhow::Result<Option<face_landmark::FaceLandmarkResult>> {
            match landmarker.as_mut() {
            Some(lm) => {
                let (r, dur) = measure_time!({
                    lm.proc_image(&src_img, crop, (crop.size.width, crop.size.height))
                        .context("关键点推理失败")?
                });
                lmk_total += dur;
                Ok(r)
            }
            None => Ok(None),
        }
    };

    if !video_mode || prev_rects.is_empty() {
        // ---- 全图检测路径（IMAGE 模式；VIDEO 模式首帧/无跟踪时）----
        // 检测器直接输出原图像素坐标
        let (dets, dur) = measure_time!({
            detector
                .proc_image(&src_img, args.conf_th, args.iou_th)
                .context("人脸检测推理失败")?
        });
        detect_dur += dur;
        let mut dets = dets;
        if args.num_faces > 0 {
            dets.sort_by(|a, b| b.conf.total_cmp(&a.conf));
            dets.truncate(args.num_faces);
        }
        for face in dets {
            let crop =
                landmark_crop_from_bbox(face.bbox, img_size, LANDMARK_CROP_MARGIN);
            let lmk = run_landmark(crop)?;
            if video_mode {
                if let Some(res) = &lmk {
                    next_rects.push(tracker::roi_from_landmarks(&res.points, crop, width, height));
                }
            }
            faces.push(FaceOut {
                bbox: face.bbox,
                conf: face.conf,
                crop,
                landmarks: face.landmarks,
                lmk,
            });
        }
    } else {
        // ---- 跟踪路径：对上一帧 ROI 直接跑 landmarks，不跑检测器 ----
        let mut dropped = 0usize;
        for rect in prev_rects {
            // 上一帧 ROI 可能来自不同尺寸的帧，先限制到当前图像范围
            let rect = tracker::clamp_rect(rect, width, height);
            match run_landmark(rect)? {
                Some(res) => {
                    let bbox = points_bbox(&res.points, rect);
                    let landmarks = landmarks_from_478(&res.points, rect);
                    next_rects.push(tracker::roi_from_landmarks(&res.points, rect, width, height));
                    faces.push(FaceOut {
                        bbox,
                        conf: res.score,
                        crop: rect,
                        landmarks,
                        lmk: Some(res),
                    });
                }
                None => dropped += 1,
            }
        }
        // 跟踪数量不足 → 本帧立即重新检测补齐（官方为下一帧补检，这里当帧补齐减少输出抖动）。
        // num_faces=0（不限）时只要有 track 丢失才重检；num_faces=N 时未达 N 就持续检测（官方语义）。
        let need_more = match args.num_faces {
            0 => faces.is_empty() || dropped > 0,
            n => faces.len() < n,
        };
        if need_more {
            let (dets, dur) = measure_time!({
                detector
                    .proc_image(&src_img, args.conf_th, args.iou_th)
                    .context("人脸检测推理失败")?
            });
            detect_dur += dur;
            let mut dets = dets;
            dets.sort_by(|a, b| b.conf.total_cmp(&a.conf));
            for face in dets {
                if args.num_faces > 0 && faces.len() >= args.num_faces {
                    break;
                }
                let crop = landmark_crop_from_bbox(face.bbox, img_size, LANDMARK_CROP_MARGIN);
                // 关联去重：与存活 track 重叠的检测视为同一张脸（官方 AssociationNormRectCalculator）
                if next_rects
                    .iter()
                    .any(|r| tracker::iou(r, &crop) > tracker::ASSOC_IOU_TH)
                {
                    continue;
                }
                let lmk = run_landmark(crop)?;
                if let Some(res) = &lmk {
                    next_rects.push(tracker::roi_from_landmarks(&res.points, crop, width, height));
                }
                faces.push(FaceOut {
                    bbox: face.bbox,
                    conf: face.conf,
                    crop,
                    landmarks: face.landmarks,
                    lmk,
                });
            }
        }
    }
    tracker.next_rects = next_rects;

    let pretty = args.pretty;
    let l0 = if pretty { "\n  " } else { "" };
    let l1 = if pretty { "\n    " } else { "" };
    let l2 = if pretty { "\n      " } else { "" };

    buf.write_all(b"{")?;
    buf.write_all(l0.as_bytes())?;
    buf.write_all(b"\"image\":\"")?;
    escape_json(&path.display().to_string(), buf)?;
    buf.write_all(b"\",")?;
    buf.write_all(l0.as_bytes())?;
    write!(buf, "\"width\":{width},")?;
    buf.write_all(l0.as_bytes())?;
    write!(buf, "\"height\":{height},")?;
    buf.write_all(l0.as_bytes())?;
    write!(buf, "\"num_faces\":{},", faces.len())?;
    buf.write_all(l0.as_bytes())?;
    buf.write_all(b"\"faces\":[")?;

    for (i, face) in faces.iter().enumerate() {
        if i > 0 {
            buf.write_all(b",")?;
        }
        buf.write_all(l1.as_bytes())?;
        buf.write_all(b"{")?;
        buf.write_all(l2.as_bytes())?;

        write!(buf, "\"confidence\":{:.4},", face.conf)?;
        buf.write_all(l2.as_bytes())?;
        write!(
            buf,
            "\"bbox\":[{:.2},{:.2},{:.2},{:.2}],",
            face.bbox.xy.x, face.bbox.xy.y, face.bbox.size.width, face.bbox.size.height
        )?;
        buf.write_all(l2.as_bytes())?;
        buf.write_all(b"\"landmarks\":[")?;
        for (j, p) in face.landmarks.iter().enumerate() {
            if j > 0 {
                buf.write_all(b",")?;
            }
            write!(buf, "[{:.2},{:.2}]", p.0, p.1)?;
        }
        buf.write_all(b"]")?;

        if landmarker.is_some() {
            buf.write_all(b",")?;
            buf.write_all(l2.as_bytes())?;
            buf.write_all(b"\"landmarks478\":")?;
            match &face.lmk {
                Some(res) => {
                    buf.write_all(b"{")?;
                    buf.write_all(l2.as_bytes())?;
                    write!(buf, "\"score\":{:.4},", res.score)?;
                    buf.write_all(l2.as_bytes())?;
                    write!(buf, "\"tongue\":{:.4},", res.tongue)?;
                    buf.write_all(l2.as_bytes())?;
                    buf.write_all(b"\"points\":[")?;
                    for (j, p) in res.points.iter().enumerate() {
                        if j > 0 {
                            buf.write_all(b",")?;
                        }
                        write!(
                            buf,
                            "[{:.2},{:.2},{:.4}]",
                            face.crop.xy.x + p.0,
                            face.crop.xy.y + p.1,
                            p.2
                        )?;
                    }
                    buf.write_all(b"]")?;

                    if let Some(bs) = blendshapes.as_mut() {
                        // blendshapes 模型输入为 146 个标记点在 256×256 裁剪空间的像素坐标
                        let (scores, dur) = measure_time!({
                            bs.proc(&res.points, face.crop.size.width)
                                .context("blendshapes 推理失败")?
                        });
                        bs_total += dur;
                        buf.write_all(b",")?;
                        buf.write_all(l2.as_bytes())?;
                        buf.write_all(b"\"blendshapes\":[")?;
                        for (k, (name, &score)) in
                            blendshapes::BLENDSHAPE_NAMES.iter().zip(scores.iter()).enumerate()
                        {
                            if k > 0 {
                                buf.write_all(b",")?;
                            }
                            buf.write_all(l2.as_bytes())?;
                            write!(buf, "{{\"name\":\"{name}\",\"score\":{score:.4}}}")?;
                        }
                        buf.write_all(l2.as_bytes())?;
                        buf.write_all(b"]")?;
                    }

                    buf.write_all(l2.as_bytes())?;
                    buf.write_all(b"}")?;
                }
                None => buf.write_all(b"null")?,
            }
        }

        buf.write_all(l1.as_bytes())?;
        buf.write_all(b"}")?;
    }

    buf.write_all(l0.as_bytes())?;
    buf.write_all(b"],")?;
    buf.write_all(l0.as_bytes())?;
    write!(
        buf,
        "\"timings_ms\":{{\"decode\":{:.2},\"detect\":{:.2}",
        decode_dur.as_secs_f32() * 1000.0,
        detect_dur.as_secs_f32() * 1000.0
    )?;
    if landmarker.is_some() {
        write!(
            buf,
            ",\"landmarks478\":{:.2}",
            lmk_total.as_secs_f32() * 1000.0
        )?;
        if blendshapes.is_some() {
            write!(
                buf,
                ",\"blendshapes\":{:.2}",
                bs_total.as_secs_f32() * 1000.0
            )?;
        }
    }
    buf.write_all(b"}")?;
    if pretty {
        buf.write_all(b"\n")?;
    }
    buf.write_all(b"}")?;

    Ok(())
}

/// 单图模式：一条结果 + 结尾换行
fn run_single(
    args: &Args,
    detector: &mut Detector,
    landmarker: &mut Option<FaceLandmark>,
    blendshapes: &mut Option<FaceBlendshapes>,
    tracker: &mut FaceTracker,
) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(64 * 1024, stdout.lock());
    let mut buf = Vec::with_capacity(64 * 1024);
    process_image(
        &args.image_path,
        args,
        detector,
        landmarker,
        blendshapes,
        tracker,
        &mut buf,
    )?;
    out.write_all(&buf)?;
    out.write_all(b"\n")?;
    out.flush().context("写出结果失败")?;
    Ok(())
}

/// 流式模式：stdin 每行一个图片路径，模型复用，适合连续视频帧。
/// 单行失败输出 {"image":...,"error":...} 并继续，不中断整个流。
fn run_stream(
    args: &Args,
    detector: &mut Detector,
    landmarker: &mut Option<FaceLandmark>,
    blendshapes: &mut Option<FaceBlendshapes>,
    tracker: &mut FaceTracker,
) -> Result<(), Error> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::with_capacity(64 * 1024, stdout.lock());
    let mut buf = Vec::with_capacity(64 * 1024);
    let stdin = std::io::stdin();
    for line in std::io::BufRead::lines(stdin.lock()) {
        let line = line.context("读取 stdin 失败")?;
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        buf.clear();
        match process_image(
            Path::new(path),
            args,
            detector,
            landmarker,
            blendshapes,
            tracker,
            &mut buf,
        ) {
            Ok(()) => {
                out.write_all(&buf)?;
                out.write_all(b"\n")?;
            }
            Err(e) => {
                out.write_all(b"{\"image\":\"")?;
                escape_json(path, &mut out)?;
                out.write_all(b"\",\"error\":\"")?;
                escape_json(&format!("{e:#}"), &mut out)?;
                out.write_all(b"\"}\n")?;
            }
        }
        out.flush()?;
    }
    Ok(())
}

fn run() -> Result<(), Error> {
    let args = parse_args().map_err(Error::msg)?;

    // 预处理的并行转换与 ORT 推理共用 CPU，线程池与推理线程数保持一致，
    // 避免 4 核机器上两套池 + 主线程互相争抢产生延迟抖动
    rayon::ThreadPoolBuilder::new()
        .num_threads(args.threads)
        .build_global()
        .ok();

    if args.tracy {
        tracy_client::Client::start();
    }
    // 必须在任何 ort API 之前调用
    if let Some(lib) = &args.ort_lib {
        // rc.13：init_from 直接加载动态库并返回 Result；commit 由首个 Session 隐式完成
        ort::init_from(lib)
            .map_err(|e| anyhow::anyhow!("加载 onnxruntime 失败: {}: {e}", lib.display()))?;
    }

    if args.segment {
        // ---- 人像分割模式 ----
        let model_path = args.weights_dir.join(args.segmenter.model_file());
        let mut segmenter = match args.segmenter {
            SegmenterKind::Selfie => Segmenter::Selfie(
                SelfieSegmenter::new(&model_path, args.ep, args.threads)
                    .with_context(|| format!("加载分割模型失败: {}", model_path.display()))?,
            ),
            SegmenterKind::Multiclass => Segmenter::Multiclass(
                facelandmark::segmenter::MulticlassSegmenter::new(&model_path, args.ep, args.threads)
                    .with_context(|| format!("加载分割模型失败: {}", model_path.display()))?,
            ),
        };
        if args.stream {
            run_segment_stream(&args, &mut segmenter)
        } else {
            run_segment_single(&args, &mut segmenter)
        }
    } else {
        // ---- 人脸检测/关键点管线 ----
        let mut detector = match args.detector {
        DetectorKind::BlazeFace => Detector::BlazeFace(
            blazeface::BlazeFace::new(
                args.weights_dir.join(DETECT_MODEL_FILE),
                args.ep,
                args.threads,
            )
            .with_context(|| {
                format!(
                    "加载检测模型失败: {}",
                    args.weights_dir.join(DETECT_MODEL_FILE).display()
                )
            })?,
        ),
        DetectorKind::YoloV5 => Detector::YoloV5(
            yolov5_face::YoloV5Face::new(
                args.weights_dir.join(YOLOV5_MODEL_FILE),
                args.ep,
                args.threads,
            )
            .with_context(|| {
                format!(
                    "加载检测模型失败: {}",
                    args.weights_dir.join(YOLOV5_MODEL_FILE).display()
                )
            })?
            .with_rgb_input(true),
        ),
    };
    let mut landmarker = if args.run_landmarks {
        Some(
            FaceLandmark::new(
                args.weights_dir.join(LANDMARK_MODEL_FILE),
                args.ep,
                args.threads,
            )
            .with_context(|| {
                format!(
                    "加载关键点模型失败: {}",
                    args.weights_dir.join(LANDMARK_MODEL_FILE).display()
                )
            })?
            .with_rgb_input(true),
        )
    } else {
        None
    };
    // blendshapes 模型可选：weights/ 下存在 face_blendshapes.onnx（或编译期
    // 嵌入副本，可执行文件单独分发时生效）时自动启用（--no-blendshapes 关闭）
    let mut blendshapes = match (&landmarker, args.run_blendshapes, args.weights_dir.join(BLENDSHAPES_MODEL_FILE)) {
        (Some(_), true, bs_path)
            if bs_path.exists() || embedded::model_bytes(BLENDSHAPES_MODEL_FILE).is_some() =>
        {
            Some(
                FaceBlendshapes::new(&bs_path, args.ep, args.threads)
                    .with_context(|| format!("加载 blendshapes 模型失败: {}", bs_path.display()))?,
            )
        }
        _ => None,
    };

    if args.stream {
        let mut tracker = FaceTracker::new();
        run_stream(
            &args,
            &mut detector,
            &mut landmarker,
            &mut blendshapes,
            &mut tracker,
        )
    } else {
        let mut tracker = FaceTracker::new();
        run_single(
            &args,
            &mut detector,
            &mut landmarker,
            &mut blendshapes,
            &mut tracker,
        )
    }
    }
}

fn main() {
    if let Err(e) = run() {
        // 下游管道提前关闭（如 `| head`）时静默退出
        let broken_pipe = e
            .chain()
            .any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe));
        if broken_pipe {
            std::process::exit(0);
        }
        eprintln!("错误: {e:#}");
        std::process::exit(1);
    }
}
