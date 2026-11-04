//! BeautyPipeline 集成测试（真实 GPU，headless）。
use beauty_filters::{BeautyPipeline, BeautyParams, GpuContext};

fn dummy_frame(w: u32, h: u32) -> Vec<u8> {
    // 彩色棋盘：亮度按格交错，且 R 随 x、B 随 y 走——红蓝不同值的图
    // 才能把通道交换（曾因此漏过 BGRA 链的红蓝反转）暴露在直通断言里。
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = ((x / 8 + y / 8) % 2) as u8 * 200 + 20;
            let r = (x * 7 % 251) as u8;
            let b = (y * 11 % 249) as u8;
            px.extend_from_slice(&[r, v, b, 255]);
        }
    }
    px
}

/// RGBA 棋盘格换成 BGRA 字节序（R 与 B 互换）。
fn dummy_frame_bgra(w: u32, h: u32) -> Vec<u8> {
    let mut px = dummy_frame(w, h);
    for p in px.chunks_mut(4) {
        p.swap(0, 2);
    }
    px
}

#[test]
fn zero_params_is_passthrough() {
    let ctx = GpuContext::new();
    let (w, h) = (320, 240);
    let input = dummy_frame(w, h);
    let mut pipe = BeautyPipeline::new(ctx, w, h);
    pipe.set_params(BeautyParams::default());
    let tex = pipe.process(&input, w, h, &[], &[]);
    assert_eq!(tex.width(), w);
    assert_eq!(tex.height(), h);
    let out = pipe.read_output_rgba();
    assert_eq!(out.len(), input.len());
    // 直通：逐像素容差比较（float shader 路径允许 ±1）。
    for (a, b) in input.iter().zip(out.iter()) {
        assert!((*a as i32 - *b as i32).abs() <= 2, "a={a} b={b}");
    }
}

/// BGRA 管线全零参数 = 精确直通：换序 pass + 最终 copy 两端字节序一致。
#[test]
fn bgra_zero_params_is_passthrough() {
    let ctx = GpuContext::new();
    let (w, h) = (320, 240);
    let input = dummy_frame_bgra(w, h);
    let mut pipe = BeautyPipeline::new_bgra(ctx, w, h);
    pipe.set_params(BeautyParams::default());
    pipe.process_bgra(&input, w, h, &[], &[]);
    let out = pipe.read_output_bgra();
    assert_eq!(out.len(), input.len());
    for (a, b) in input.iter().zip(out.iter()) {
        assert!((*a as i32 - *b as i32).abs() <= 2, "a={a} b={b}");
    }
}

/// 回归测试（Critical 修复）：首帧即传非零 reshape 控制点，滤镜必须在首帧收到。
/// 修复前 `mem::replace` 把旧值（首帧为空）传给滤镜 → has_face=false → 直通 → 输出 == 输入，
/// 修复后滤镜首帧收到 landmark → 变形 → 输出 != 输入。
#[test]
fn first_frame_landmarks_reach_filters() {
    let ctx = GpuContext::new();
    let (w, h) = (320, 240);
    let input = dummy_frame(w, h);
    let mut pipe = BeautyPipeline::new(ctx, w, h);
    // makeup 全 0（212 f32 满足 has_face 长度要求；blusher=0 不影响输出）。
    let makeup: Vec<f32> = vec![0.0; 212];
    // reshape 212 f32：以 (0.5, 0.5) 为中心逐点确定性偏移，
    // 保证 shader 读取的每对 origin/target 控制点不同（否则 delta 抵消、半径 0 退化）。
    let reshape: Vec<f32> = (0..106)
        .flat_map(|i| {
            let dx = ((i * 3) % 11) as f32 / 10.0 - 0.5;
            let dy = ((i * 7) % 11) as f32 / 10.0 - 0.5;
            [0.5 + 0.15 * dx, 0.5 + 0.15 * dy]
        })
        .collect();
    // 仅瘦脸/大眼生效：磨皮、美白、腮红全 0（全 0 时 GPU 链为精确直通，
    // 若首帧 landmark 未达滤镜，输出将逐字节等于输入）。
    pipe.set_params(BeautyParams {
        smoothing: 0.0,
        whiten: 0.0,
        face_slim: 0.6,
        eye_zoom: 0.2,
        blusher: 0.0,
    });
    pipe.process(&input, w, h, &makeup, &reshape);
    let out = pipe.read_output_rgba();
    assert_ne!(out, input, "首帧 landmark 必须到达 reshape 滤镜并产生变形");
}

#[test]
fn nonzero_params_change_output() {
    let ctx = GpuContext::new();
    let (w, h) = (320, 240);
    let input = dummy_frame(w, h);
    let mut pipe = BeautyPipeline::new(ctx, w, h);
    // 106 点 makeup 网格 + 17 控制点 reshape（全 0 满足长度要求，
    // bbox 为 0 宽 → 基础半径；控制点全 0 → delta 0）。
    let makeup: Vec<f32> = vec![0.0; 212];
    let reshape: Vec<f32> = vec![0.0; 212];
    pipe.set_params(BeautyParams {
        smoothing: 0.5,
        whiten: 0.5,
        face_slim: 0.3,
        eye_zoom: 0.2,
        blusher: 0.5,
    });
    pipe.process(&input, w, h, &makeup, &reshape);
    let out = pipe.read_output_rgba();
    assert_ne!(out, input, "美颜参数非零时输出必须变化");
}
