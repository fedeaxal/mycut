//! 库用法示例：加载各模型并对一张图片执行人脸检测 / 关键点 / blendshapes / 人像分割。
//!
//! 运行（需先有 onnxruntime 动态库）:
//!   cargo run --example faces_demo -- photos.jpg
//!   ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.dylib cargo run --example faces_demo

use fast_image_resize::{PixelType, images::ImageRef};

use facelandmark::blazeface::BlazeFace;
use facelandmark::blendshapes::FaceBlendshapes;
use facelandmark::face_landmark::FaceLandmark;
use facelandmark::geometry::Rect;
use facelandmark::segmenter::SelfieSegmenter;
use facelandmark::utils::Ep;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "screenshots/screenshot.png".into());
    let img = image::open(&path)?;
    let (w, h) = (img.width(), img.height());
    let rgb = img.into_rgb8();
    let src = ImageRef::new(w, h, rgb.as_raw(), PixelType::U8x3)?;

    let (ep, threads) = (Ep::Auto, facelandmark::utils::default_threads());

    // 人脸检测（BlazeFace，坐标为原图像素）
    let mut detector = BlazeFace::new("weights/blazeface_short_range.onnx", ep, threads)?;
    let dets = detector.proc_image(&src, 0.6, 0.5)?;
    println!("检测到 {} 张脸", dets.len());

    for d in dets.iter().take(1) {
        let Rect { xy, size } = d.bbox;
        println!(
            "face conf={:.3} bbox=({:.0},{:.0},{:.0}x{:.0})",
            d.conf, xy.x, xy.y, size.width, size.height
        );

        // 478 点关键点：对检测框外扩 1.5 倍的正方形区域推理
        let side = size.width.max(size.height) * 1.5;
        let left = (xy.x + size.width / 2.0 - side / 2.0).clamp(0.0, w as f32 - side);
        let top = (xy.y + size.height / 2.0 - side / 2.0).clamp(0.0, h as f32 - side);
        let crop = Rect {
            xy: (left, top).into(),
            size: (side, side).into(),
        };
        let mut landmarker =
            FaceLandmark::new("weights/face_landmarks_detector.onnx", ep, threads)?
                .with_rgb_input(true);
        if let Some(lm) = landmarker.proc_image(&src, crop, (side, side))? {
            println!("landmarks478 score={:.3} points={}", lm.score, lm.points.len());

            // 52 个表情系数（blendshapes）
            let mut bs = FaceBlendshapes::new(
                "weights/face_blendshapes.onnx",
                ep,
                threads,
            )?;
            let scores = bs.proc(&lm.points, side)?;
            let mut top: Vec<_> = facelandmark::blendshapes::BLENDSHAPE_NAMES
                .iter()
                .zip(&scores)
                .collect();
            top.sort_by(|a, b| b.1.total_cmp(a.1));
            for (name, s) in top.iter().take(3) {
                println!("  blendshape {name}={s:.3}");
            }
        }
    }

    // 人像分割：selfie（最快）/ multiclass（质量更高，可从 SegmenterKind 切换）
    let mut segmenter = SelfieSegmenter::new("weights/selfie_segmenter.onnx", ep, threads)?;
    let mask = segmenter.proc_image(&src)?;
    let cover: f32 = mask.data.iter().filter(|&&v| v > 0.5).count() as f32 / mask.data.len() as f32;
    println!("分割掩码 {}x{} 人像占比 {:.1}%", mask.width, mask.height, cover * 100.0);

    Ok(())
}
