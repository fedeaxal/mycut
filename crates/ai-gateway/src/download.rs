//! 按需下载器（自 main/asr-models.ts + web-fonts.ts 逐语义移植）：
//! ASR 识别模型（ModelScope modelscope 库，三个 model_id 顺序回退）与
//! 界面扩展字体（jsdelivr 目录，ureq）。模型下载不再随启动/录制自动
//! 触发——由 AI 设置框的用户动作经 download("asr_model_ensure") 进入；
//! 字体保持启动检查、缺失续传。.part 原子 rename、content-length 校验、
//! manifest 字节数完整性判定与进度事件（500ms 节流）契约不变。

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

// 模型仓库（know-how，编进二进制）：不能用 ModelScope 官方仓库
// iic/SenseVoiceSmall-onnx——那是 FunASR 原生导出，缺 sherpa-onnx 要求
// 的 ONNX metadata，加载直接报错。以下均为 sherpa-onnx 官方 release
// sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17 的解包镜像，都含
// model.int8.onnx 与 tokens.txt；按顺序尝试，前一个失败换下一个。
pub const ASR_MODEL_IDS: &[&str] = &[
    "pengzhendong/sherpa-onnx-sense-voice-zh-en-ja-ko-yue",
    "mariolux/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17",
    "poloniumrock/SenseVoiceSmallOnnx",
];
pub const ASR_MODEL_FILES: &[&str] = &["model.int8.onnx", "tokens.txt"];

/// 界面字体目录（family, jsdelivr 直链）。新增字体在此追加。
pub const FONT_CATALOG: &[(&str, &str)] = &[
    ("包图小白体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E5%8C%85%E5%9B%BE%E5%B0%8F%E7%99%BD%E4%BD%93.ttf"),
    ("摄图摩登小方体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%91%84%E5%9B%BE%E6%91%A9%E7%99%BB%E5%B0%8F%E6%96%B9%E4%BD%93.ttf"),
    ("沐瑶软笔手写体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E6%B2%90%E7%91%B6%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E6%B2%90%E7%91%B6%E8%BD%AF%E7%AC%94%E6%89%8B%E5%86%99%E4%BD%93.ttf"),
    ("沐瑶随心手写体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E6%B2%90%E7%91%B6%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E6%B2%90%E7%91%B6%E9%9A%8F%E5%BF%83%E6%89%8B%E5%86%99%E4%BD%93.ttf"),
    ("千图小兔体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E5%8D%83%E5%9B%BE%E5%B0%8F%E5%85%94%E4%BD%93.ttf"),
    ("庞门正道粗书体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%BA%9E%E9%97%A8%E6%AD%A3%E9%81%93%E7%B2%97%E4%B9%A6%E4%BD%93%E7%B3%BB%E5%88%97/%E5%BA%9E%E9%97%A8%E6%AD%A3%E9%81%93%E7%B2%97%E4%B9%A6%E4%BD%93.ttf"),
    ("庞门正道标题体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%BA%9E%E9%97%A8%E6%AD%A3%E9%81%93%E6%A0%87%E9%A2%98%E4%BD%93%E7%B3%BB%E5%88%97/%E5%BA%9E%E9%97%A8%E6%AD%A3%E9%81%93%E6%A0%87%E9%A2%98%E4%BD%93.ttf"),
    ("钟齐志莽行书", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E9%92%9F%E9%BD%90%E5%BF%97%E8%8E%BD%E8%A1%8C%E4%B9%A6.ttf"),
    ("站酷高端黑", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E7%AB%99%E9%85%B7%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E7%AB%99%E9%85%B7%E9%AB%98%E7%AB%AF%E9%BB%91.ttf"),
    ("站酷文艺体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E7%AB%99%E9%85%B7%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E7%AB%99%E9%85%B7%E6%96%87%E8%89%BA%E4%BD%93.ttf"),
    ("站酷酷黑体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E7%AB%99%E9%85%B7%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E7%AB%99%E9%85%B7%E9%85%B7%E9%BB%91%E4%BD%93.ttf"),
    ("站酷快乐体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E7%AB%99%E9%85%B7%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E7%AB%99%E9%85%B7%E5%BF%AB%E4%B9%90%E4%BD%93.ttf"),
    ("胡晓波男神体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E8%83%A1%E6%99%93%E6%B3%A2%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E8%83%A1%E6%99%93%E6%B3%A2%E7%94%B7%E7%A5%9E%E4%BD%93.otf"),
    ("胡晓波骚包体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E8%83%A1%E6%99%93%E6%B3%A2%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E8%83%A1%E6%99%93%E6%B3%A2%E9%AA%9A%E5%8C%85%E4%BD%93.otf"),
    ("胡晓波真帅体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E8%83%A1%E6%99%93%E6%B3%A2%E5%AD%97%E4%BD%93%E7%B3%BB%E5%88%97/%E8%83%A1%E6%99%93%E6%B3%A2%E7%9C%9F%E5%B8%85%E4%BD%93.otf"),
    ("演示佛系体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%BC%94%E7%A4%BA%E4%BD%9B%E7%B3%BB%E4%BD%93.ttf"),
    ("演示秋鸿楷", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%BC%94%E7%A4%BA%E7%A7%8B%E9%B8%BF%E6%A5%B7.ttf"),
    ("演示夏行楷", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%BC%94%E7%A4%BA%E5%A4%8F%E8%A1%8C%E6%A5%B7.ttf"),
    ("演示春风楷", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%BC%94%E7%A4%BA%E6%98%A5%E9%A3%8E%E6%A5%B7.ttf"),
    ("演示悠然小楷", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%BC%94%E7%A4%BA%E6%82%A0%E7%84%B6%E5%B0%8F%E6%A5%B7.ttf"),
    ("江西拙楷", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%B1%9F%E8%A5%BF%E6%8B%99%E6%A5%B7.ttf"),
    ("优设标题黑", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E4%BC%98%E8%AE%BE%E6%A0%87%E9%A2%98%E9%BB%91.ttf"),
    ("联盟起艺卢帅正锐黑体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E8%81%94%E7%9B%9F%E8%B5%B7%E8%89%BA%E5%8D%A2%E5%B8%85%E6%AD%A3%E9%94%90%E9%BB%91%E4%BD%93.ttf"),
    ("新叶念体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%96%B0%E5%8F%B6%E5%BF%B5%E4%BD%93.otf"),
    ("手书体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%89%8B%E4%B9%A6%E4%BD%93.ttf"),
    ("杨任东竹石体Bold", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E6%9D%A8%E4%BB%BB%E4%B8%9C%E7%AB%B9%E7%9F%B3%E4%BD%93/%E6%9D%A8%E4%BB%BB%E4%B8%9C%E7%AB%B9%E7%9F%B3%E4%BD%93-Bold.ttf"),
    ("卓健橄榄简体", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E5%8D%93%E5%81%A5%E6%A9%84%E6%A6%84%E7%AE%80%E4%BD%93.ttf"),
    ("OPPOSans-H", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/OPPOSans/OPPOSans-H.ttf"),
    ("OPPOSans-M", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/OPPOSans/OPPOSans-M.ttf"),
    ("OPPOSans-R", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/OPPOSans/OPPOSans-R.ttf"),
    ("阿里巴巴普惠体-Heavy", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93-Heavy.ttf"),
    ("阿里巴巴普惠体-Bold", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93-Bold.ttf"),
    ("阿里巴巴普惠体-Medium", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93-Medium.ttf"),
    ("阿里巴巴普惠体-Regular", "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93/%E9%98%BF%E9%87%8C%E5%B7%B4%E5%B7%B4%E6%99%AE%E6%83%A0%E4%BD%93-Regular.ttf"),
    // Google Fonts 开源中文字体（封面/艺术字场景），jsdelivr 直链 google/fonts
    ("马善政毛笔楷书", "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/mashanzheng/MaShanZheng-Regular.ttf"),
    ("站酷庆科黄油体", "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/zcoolqingkehuangyou/ZCOOLQingKeHuangYou-Regular.ttf"),
    ("站酷小薇", "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/zcoolxiaowei/ZCOOLXiaoWei-Regular.ttf"),
    ("龙藏", "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/longcang/LongCang-Regular.ttf"),
    ("柳建毛草", "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/liujianmaocao/LiuJianMaoCao-Regular.ttf"),
    ("志莽行书", "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/zhimangxing/ZhiMangXing-Regular.ttf"),
];

const MANIFEST_NAME: &str = "manifest.json";
const STALL_SECS: u64 = 60;
const PROGRESS_EMIT_MS: u128 = 500;
// ModelScope 的 LFS CDN 拒绝无 User-Agent 的请求（HTTP 403），与旧实现
// 相同的浏览器 UA
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// URL percent-decode（字体文件名的 URL 编码中文还原）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// URL 的 path 部分（JS new URL(url).pathname，不含 query/hash）。
fn url_path(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let with_query = after_scheme.split_once('/').map(|(_, p)| p).unwrap_or("");
    let without_query = with_query.split(['?', '#']).next().unwrap_or("");
    format!("/{without_query}")
}

/// 模型下载失败聚合里的短名（model_id 首段 = 仓库属主）。
fn model_owner(model_id: &str) -> String {
    model_id.split('/').next().unwrap_or(model_id).to_string()
}

/// 字体落盘文件名：family + URL 末段扩展名（decode、小写；无扩展名兜底 .ttf）。
fn font_file_name(family: &str, url: &str) -> String {
    let path = url_path(url);
    let last = path.split('/').last().unwrap_or("");
    let file = percent_decode(last);
    let ext = match file.rfind('.') {
        Some(dot) => file[dot..].to_lowercase(),
        None => ".ttf".to_string(),
    };
    format!("{family}{ext}")
}

fn font_entries() -> Vec<(String, String, String)> {
    FONT_CATALOG
        .iter()
        .map(|(family, url)| {
            (
                family.to_string(),
                url.to_string(),
                font_file_name(family, url),
            )
        })
        .collect()
}

/// manifest 字节数完整性：全部匹配才算完整（下载中断不会写 manifest，
/// 磁盘损坏/手动删改导致大小不符也会被判为不完整）。
fn is_complete(dir: &Path, files: &[String]) -> bool {
    let manifest: Value = match std::fs::read_to_string(dir.join(MANIFEST_NAME)) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let Some(sizes) = manifest["files"].as_object() else {
        return false;
    };
    for name in files {
        let Some(expected) = sizes.get(name.as_str()).and_then(|v| v.as_u64()) else {
            return false;
        };
        let Ok(meta) = std::fs::metadata(dir.join(name)) else {
            return false;
        };
        if meta.len() != expected {
            return false;
        }
    }
    true
}

/// manifest 只记给定文件集的字节数（全部落盘成功后才写）。
fn write_manifest(dir: &Path, files: &[String]) -> Result<(), String> {
    let mut sizes = serde_json::Map::new();
    for name in files {
        let size = std::fs::metadata(dir.join(name))
            .map_err(|e| e.to_string())?
            .len();
        sizes.insert(name.clone(), json!(size));
    }
    std::fs::write(
        dir.join(MANIFEST_NAME),
        json!({ "files": sizes, "downloadedAt": utc_now_iso() }).to_string(),
    )
    .map_err(|e| e.to_string())
}

/// UTC ISO-8601（与 JS toISOString 同形），days→civil 为 Hinnant 算法。
fn utc_now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let days = (secs / 86_400) as i64;
    let (hh, mm, ss) = (
        (secs % 86_400) / 3600,
        (secs % 3600) / 60,
        secs % 60,
    );
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// HTTPS 客户端：无默认特性的 ureq 3 无内置 TLS，统一纯 Rust rustls +
/// 系统证书（与工作区其余 ureq 使用方同一套栈）。connect/读超时分别
/// 对应旧实现的连接超时与停滞看门狗。
fn agent(connect: Duration, read_body: Option<Duration>) -> ureq::Agent {
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .unversioned_rustls_crypto_provider(provider)
                .build(),
        )
        .http_status_as_error(false)
        .timeout_connect(Some(connect))
        .timeout_recv_body(read_body)
        .build()
        .into()
}

/// 单文件流式下载：.part 原子 rename；连接 60s / 单次读 60s 超时
///（等价旧实现的响应头看门狗 + 停滞看门狗）；content-length 校验。
fn download_file(
    url: &str,
    dest: &Path,
    on_progress: &dyn Fn(u64, u64),
) -> Result<u64, String> {
    // connect 超时须短于 read 看门狗：字体下载的主域名回退等的就是
    // 连接失败（对黑洞地址 60s 会让串行队列每个字体白等一分钟），
    // 快速失败换下家；读超时 60s 仍是下载中停滞的看门狗
    let agent = agent(Duration::from_secs(12), Some(Duration::from_secs(STALL_SECS)));
    let response = agent
        .get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|error| match error {
            ureq::Error::Timeout(_) => "下载停滞（60s 无数据）".to_string(),
            other => other.to_string(),
        })?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}：{url}"));
    }
    let declared = response
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let part_path = format!("{}.part", dest.display());
    let cleanup = || {
        let _ = std::fs::remove_file(&part_path);
    };
    let mut reader = response.into_body().into_reader();
    let mut file = match std::fs::File::create(&part_path) {
        Ok(f) => f,
        Err(error) => return Err(error.to_string()),
    };
    let mut received: u64 = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(error) = file.write_all(&buffer[..n]) {
                    drop(file);
                    cleanup();
                    return Err(error.to_string());
                }
                received += n as u64;
                on_progress(received, declared);
            }
            Err(error) => {
                drop(file);
                cleanup();
                // 读超时 = 旧实现的停滞看门狗语义
                if error.kind() == std::io::ErrorKind::TimedOut
                    || error.to_string().contains("timed out")
                {
                    return Err("下载停滞（60s 无数据）".to_string());
                }
                return Err(error.to_string());
            }
        }
    }
    if declared > 0 && received != declared {
        drop(file);
        cleanup();
        return Err(format!("下载不完整：{received}/{declared}"));
    }
    drop(file);
    std::fs::rename(&part_path, dest).map_err(|error| error.to_string())?;
    Ok(received)
}

/// 下载临时根：modelscope 落盘到 <stage_root>/<model_id>/，成功后把
/// 模型文件 move 进最终目录并写 manifest。临时树在成功前保留——
/// modelscope 按 Range 断点续传，下次下载同 model_id 从已下载字节继续。
fn model_stage_root(dir: &Path) -> PathBuf {
    dir.with_file_name(format!(
        "{}.downloading",
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("asr-models")
    ))
}

/// 预取单文件大小。主站的 HEAD 不跟随 302 且无 Content-Length，改用
/// GET + Range: bytes=0-0：跟随 302 到 CDN 后返回 206，总长在
/// Content-Range（"bytes 0-0/239233841"）；CDN 不支持 Range 时退回
/// Content-Length（200 全量响应体随 response 丢弃）。拿不到返回 0
///（进度退化为只显示已下载 MB）。
fn remote_file_size(model_id: &str, file: &str) -> u64 {
    let url = format!("https://modelscope.cn/models/{model_id}/resolve/master/{file}");
    let response = match agent(Duration::from_secs(STALL_SECS), None)
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Range", "bytes=0-0")
        .call()
    {
        Ok(response) => response,
        Err(_) => return 0,
    };
    if let Some(range) = response.headers().get("Content-Range").and_then(|v| v.to_str().ok()) {
        if let Some(total) = range.rsplit('/').next() {
            if let Ok(total) = total.trim().parse::<u64>() {
                return total;
            }
        }
    }
    response
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
}

/// 进度轮询线程：modelscope 库的下载 API 无进度回调（进度条面向 CLI
/// 终端），watcher 每 500ms 对落盘文件 stat——断点续传直接写目标文件，
/// received 天然从续传点起算。
///
/// 事件为**两文件聚合进度**（received/total 求和、file 取当前未完成的
/// 文件名）：per-file 逐条上报会让小文件（tokens.txt 秒完）以 100%/0MB
/// 形态反复覆盖主文件的真实进度，UI 表现为进度在两个文件间闪烁。
/// received 无变化时跳过（避免下载停滞期重复刷同一事件）。
fn spawn_download_watcher(
    model_dir: PathBuf,
    files: Vec<String>,
    totals: Vec<u64>,
    emit: Arc<dyn Fn(Value) + Send + Sync>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut last_emitted = 0u64;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(PROGRESS_EMIT_MS as u64));
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let mut received_sum = 0u64;
            let mut total_sum = 0u64;
            let mut active_file: Option<&String> = None;
            for (file, total) in files.iter().zip(totals.iter()) {
                let size = std::fs::metadata(model_dir.join(file))
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                received_sum += size;
                total_sum += total;
                // total 预取失败（0）视为永远"未完成"，保住文件名显示
                if *total == 0 || size < *total {
                    active_file = Some(file);
                }
            }
            if received_sum == 0 || received_sum == last_emitted {
                continue;
            }
            last_emitted = received_sum;
            emit(json!({
                "event": "status",
                "state": "downloading",
                "file": active_file.unwrap_or(&files[0]).clone(),
                "received": received_sum,
                "total": total_sum,
            }));
        }
    })
}

/// 确保模型可用（asr_model_ensure）：完整则 ready；否则按 model_id
/// 顺序经 modelscope 库下载（Range 断点续传、并发拉取两个文件），
/// 前一个失败换下一个。调用方负责并发去重——下载不随启动/录制自动
/// 触发，只能来自 AI 设置框的用户动作。
pub fn ensure_asr_model(dir: &Path, emit: Arc<dyn Fn(Value) + Send + Sync>) -> Result<String, String> {
    let files: Vec<String> = ASR_MODEL_FILES
        .iter()
        .map(|s| s.to_string())
        .collect();
    if is_complete(dir, &files) {
        emit(json!({ "event": "status", "state": "ready" }));
        return Ok("ready".to_string());
    }
    emit(json!({ "event": "status", "state": "downloading", "file": "", "received": 0, "total": 0 }));

    let stage_root = model_stage_root(dir);
    std::fs::create_dir_all(&stage_root).map_err(|e| e.to_string())?;
    // modelscope 的 API 是 async（reqwest/tokio）：下载线程内自建
    // runtime block_on 执行；runtime 内部 spawn 的文件任务由 worker
    // 线程池并发驱动
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;

    let mut errors: Vec<String> = Vec::new();
    for model_id in ASR_MODEL_IDS {
        let model_dir = stage_root.join(model_id);
        let totals: Vec<u64> = ASR_MODEL_FILES
            .iter()
            .map(|file| remote_file_size(model_id, file))
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = spawn_download_watcher(
            model_dir.clone(),
            files.clone(),
            totals,
            Arc::clone(&emit),
            Arc::clone(&stop),
        );
        let result = runtime.block_on(modelscope::ModelScope::download_with_options(
            model_id,
            stage_root.clone(),
            modelscope::DownloadOptions {
                files: Some(files.clone()),
            },
        ));
        stop.store(true, Ordering::Relaxed);
        let _ = watcher.join();
        if let Err(error) = result {
            errors.push(format!("{}: {error}", model_owner(model_id)));
            continue;
        }
        // 全部文件落盘成功：move 进最终目录 + 写 manifest + 清理临时树
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        for file in &files {
            let staged = model_dir.join(file);
            let dest = dir.join(file);
            let _ = std::fs::remove_file(&dest);
            std::fs::rename(&staged, &dest)
                .map_err(|e| format!("模型文件归位失败: {e}"))?;
        }
        write_manifest(dir, &files)?;
        let _ = std::fs::remove_dir_all(&stage_root);
        emit(json!({ "event": "status", "state": "ready" }));
        return Ok("downloaded".to_string());
    }
    Err(errors.join("；"))
}

/// 模型完整性检测（绝不触发下载）：启动时只查状态，是否下载由用户
/// 在 AI 设置框里决定。
pub fn asr_model_status(dir: &Path) -> bool {
    is_complete(
        dir,
        &ASR_MODEL_FILES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
}

/// jsdelivr 的多个服务入口：直连主域名（cdn.jsdelivr.net）在国内网络
/// 常被重置（SSL 握手失败），同名镜像入口路径结构完全一致，逐个回退。
const FONT_CDN_FALLBACKS: &[&str] = &[
    "cdn.jsdelivr.net",
    "fastly.jsdelivr.net",
    "testingcf.jsdelivr.net",
    "gcore.jsdelivr.net",
];

/// 上次成功使用的字体入口（进程级记忆）：首个字体探测到可达入口后，
/// 其余字体直接从它开始，不再逐个等不可达入口的连接超时。
static FONT_CDN_PREFERRED: AtomicUsize = AtomicUsize::new(0);

/// 字体 URL 的镜像变体序列（配合 FONT_CDN_PREFERRED 环形轮询）：
/// - jsdelivr 四入口：域名替换，路径不变；
/// - ghfast.top（GitHub 加速代理，用户钦定入口）：jsdelivr 的 gh 路径
///   还原成 github raw 直链再套代理前缀——
///   `cdn.jsdelivr.net/gh/<owner>/<repo>@<rev>/<path>`
///   → `ghfast.top/https://github.com/<owner>/<repo>/raw/refs/heads/<rev>/<path>`
///   （必须用 refs/heads 规范路径：`raw/<branch>` 在 github.com 是 302
///   跳 raw.githubusercontent.com，代理不跟随重定向时拿不到文件；
///   refs/heads 直接 200 返回内容。清单当前全是 @main 分支形态）
///   （该代理在国内部分网络可达、部分网络超时，作为回退链一员）。
fn font_url_variants(url: &str) -> Vec<String> {
    let mut variants: Vec<String> = FONT_CDN_FALLBACKS
        .iter()
        .map(|host| url.replacen("cdn.jsdelivr.net", host, 1))
        .collect();
    if let Some(rest) = url.strip_prefix("https://cdn.jsdelivr.net/gh/") {
        // rest = "<owner>/<repo>@<rev>/<path>"（owner、repo 各一段，
        // path 为剩余全部、含内部斜杠）
        let mut parts = rest.splitn(3, '/');
        if let (Some(owner), Some(repo_rev), Some(path)) =
            (parts.next(), parts.next(), parts.next())
        {
            if let Some((repo, rev)) = repo_rev.split_once('@') {
                variants.push(format!(
                    "https://ghfast.top/https://github.com/{owner}/{repo}/raw/refs/heads/{rev}/{path}"
                ));
            }
        }
    }
    variants
}

/// 字体下载：按镜像变体顺序回退（从上次成功的入口开始）；
/// 非 jsdelivr 源单次尝试。
fn download_font_file(url: &str, dest: &Path) -> Result<u64, String> {
    if !url.contains("cdn.jsdelivr.net") {
        return download_file(url, dest, &|_, _| {});
    }
    let variants = font_url_variants(url);
    let mut errors: Vec<String> = Vec::new();
    let count = variants.len();
    let start = FONT_CDN_PREFERRED.load(Ordering::Relaxed).min(count - 1);
    for offset in 0..count {
        let index = (start + offset) % count;
        match download_file(&variants[index], dest, &|_, _| {}) {
            Ok(size) => {
                FONT_CDN_PREFERRED.store(index, Ordering::Relaxed);
                return Ok(size);
            }
            Err(error) => errors.push(format!("[{}] {}", variants[index], error)),
        }
    }
    Err(errors.join("；"))
}

/// 连续这么多个字体全军覆没（五个镜像/变体都拿不到）即判定当前网络
/// 对整个字体 CDN 不可达,提前终止本轮下载——不然四十个字体会把全部
/// 镜像轮询一遍,反复的连接/TLS 握手白烧 CPU 与时间,下一轮启动再试。
const MAX_CONSECUTIVE_FONT_FAILURES: usize = 3;

/// 确保字体可用（web_fonts_ensure）：manifest 全量校验通过则直接返回；
/// 否则逐个补下（已落盘跳过），每完成一个发 font_ready；单个失败静默
/// 跳过（font_failed 事件供主进程日志）；连续 MAX_CONSECUTIVE_FONT_FAILURES
/// 个失败则提前终止（网络对 CDN 不可达,重试留给下次启动）;manifest 只记
/// 成功子集,下次启动只重试缺失的。
pub fn ensure_web_fonts(dir: &Path, emit: &dyn Fn(Value)) -> Result<String, String> {
    let entries = font_entries();
    let file_names: Vec<String> = entries.iter().map(|(_, _, f)| f.clone()).collect();
    if is_complete(dir, &file_names) {
        emit(json!({ "event": "status", "state": "ready" }));
        return Ok("ready".to_string());
    }
    emit(json!({ "event": "status", "state": "downloading" }));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut available: Vec<String> = Vec::new();
    let mut consecutive_failures = 0usize;
    for (family, url, file) in &entries {
        let dest = dir.join(file);
        let ready = match std::fs::metadata(&dest) {
            Ok(_) => true, // .part 原子 rename，正式文件即完整
            Err(_) => match download_font_file(url, &dest) {
                Ok(_) => true,
                Err(error) => {
                    emit(json!({
                        "event": "font_failed",
                        "family": family,
                        "error": error,
                    }));
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_FONT_FAILURES {
                        let done = available.len();
                        emit(json!({
                            "event": "status",
                            "state": "aborted",
                            "fetched": done,
                            "error": format!("连续 {consecutive_failures} 个字体下载失败,本轮跳过其余字体"),
                        }));
                        return Err(format!(
                            "字体 CDN 不可达（连续 {consecutive_failures} 个失败,已取 {done} 个）"
                        ));
                    }
                    continue;
                }
            },
        };
        if ready {
            consecutive_failures = 0;
            available.push(file.clone());
            emit(json!({ "event": "font_ready", "family": family }));
        }
    }
    if !available.is_empty() {
        write_manifest(dir, &available)?;
    }
    emit(json!({ "event": "status", "state": "ready" }));
    Ok("downloaded".to_string())
}

/// 已下载字体信息（渲染端注册 FontFace 用）：正式文件存在即完整。
pub fn list_downloaded_fonts(dir: &Path) -> Vec<(String, String)> {
    let mut available = Vec::new();
    for (family, _, file) in font_entries() {
        if std::fs::metadata(dir.join(&file)).is_ok() {
            available.push((family, dir.join(file).display().to_string()));
        }
    }
    available
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decodes_utf8_paths() {
        assert_eq!(percent_decode("%E5%8C%85%E5%9B%BE%E5%B0%8F%E7%99%BD%E4%BD%93.ttf"), "包图小白体.ttf");
        assert_eq!(percent_decode("OPPOSans-H.ttf"), "OPPOSans-H.ttf");
        assert_eq!(percent_decode("100%.ttf"), "100%.ttf"); // 非法转义原样保留
    }

    #[test]
    fn font_file_name_semantics() {
        let (family, url) = FONT_CATALOG[0];
        assert_eq!(font_file_name(family, url), "包图小白体.ttf");
        // 无扩展名兜底 .ttf
        assert_eq!(
            font_file_name("字体", "https://x.test/a/b/%E5%AD%97%E4%BD%93"),
            "字体.ttf"
        );
        // 扩展名取最后一个点并小写化，family 独立于 URL 文件名
        assert_eq!(
            font_file_name("杨任东竹石体Bold", "https://x.test/a/b/%E5%AD%97%E4%BD%93.TTF"),
            "杨任东竹石体Bold.ttf"
        );
    }

    #[test]
    fn model_owner_extracts_owner() {
        assert_eq!(model_owner(ASR_MODEL_IDS[0]), "pengzhendong");
        assert_eq!(model_owner(ASR_MODEL_IDS[1]), "mariolux");
        assert_eq!(model_owner(ASR_MODEL_IDS[2]), "poloniumrock");
        // 无斜杠的 id 原样返回
        assert_eq!(model_owner("solo"), "solo");
    }

    #[test]
    fn model_stage_root_sits_beside_final_dir() {
        let stage = model_stage_root(Path::new("/x/userData/asr-models"));
        assert_eq!(
            stage,
            PathBuf::from("/x/userData/asr-models.downloading")
        );
    }

    #[test]
    fn font_url_variants_cover_jsdelivr_and_ghfast() {
        let free_font = "https://cdn.jsdelivr.net/gh/jaywcjlove/free-font@main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E5%8C%85%E5%9B%BE%E5%B0%8F%E7%99%BD%E4%BD%93.ttf";
        let variants = font_url_variants(free_font);
        assert_eq!(variants.len(), 5);
        assert!(variants[0].starts_with("https://cdn.jsdelivr.net/gh/"));
        assert!(variants[1].starts_with("https://fastly.jsdelivr.net/gh/"));
        assert!(variants[2].starts_with("https://testingcf.jsdelivr.net/gh/"));
        assert!(variants[3].starts_with("https://gcore.jsdelivr.net/gh/"));
        // ghfast 变体：gh 路径还原成 github raw 直链（refs/heads 规范
        // 路径）再套代理前缀
        assert_eq!(
            variants[4],
            "https://ghfast.top/https://github.com/jaywcjlove/free-font/raw/refs/heads/main/docs/fonts/%E5%85%B6%E4%BB%96%E5%AD%97%E4%BD%93/%E5%8C%85%E5%9B%BE%E5%B0%8F%E7%99%BD%E4%BD%93.ttf"
        );

        // google/fonts 的 ofl 路径（无 docs 段）同构适用
        let google = "https://cdn.jsdelivr.net/gh/google/fonts@main/ofl/longcang/LongCang-Regular.ttf";
        let variants = font_url_variants(google);
        assert_eq!(
            variants[4],
            "https://ghfast.top/https://github.com/google/fonts/raw/refs/heads/main/ofl/longcang/LongCang-Regular.ttf"
        );
    }

    #[test]
    fn manifest_roundtrip_and_incomplete_detection() {
        let dir = std::env::temp_dir().join(format!("ai-gw-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = vec!["a.bin".to_string(), "b.bin".to_string()];
        std::fs::write(dir.join("a.bin"), b"12345").unwrap();
        // manifest 缺失 → 不完整
        assert!(!is_complete(&dir, &files));
        std::fs::write(dir.join("b.bin"), b"xy").unwrap();
        write_manifest(&dir, &files).unwrap();
        assert!(is_complete(&dir, &files));
        // 大小不符（磁盘损坏/手动删改）→ 不完整
        std::fs::write(dir.join("b.bin"), b"xyz").unwrap();
        assert!(!is_complete(&dir, &files));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utc_iso_shape() {
        let iso = utc_now_iso();
        // 2026-10-10T08:00:00.000Z 形
        assert_eq!(iso.len(), 24);
        assert!(iso.ends_with('Z'));
        assert_eq!(&iso[4..5], "-");
        assert_eq!(&iso[10..11], "T");
    }

    #[test]
    fn catalog_integrity() {
        // 目录非空、URL 全部 https 直链、文件名无重复
        assert!(FONT_CATALOG.len() >= 32);
        let mut names: Vec<String> = Vec::new();
        for (family, url) in FONT_CATALOG {
            assert!(url.starts_with("https://"));
            names.push(font_file_name(family, url));
            assert!(!family.is_empty());
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), FONT_CATALOG.len());
    }
}
