#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::providers::spawn_line::{MapLine, SpawnLineOptions, SpawnLineProvider};
use crate::types::{Provider, RawSample};
use crate::util::now_ms;

/**
 * 数据源：vendored mediaremote-adapter（ungive/mediaremote-adapter，BSD-3-Clause，
 * 随包内嵌分发，见 vendor/macos/ 与 LICENSE-MediaRemoteAdapter）。
 * 通过 /usr/bin/perl 动态加载自带的 MediaRemoteAdapter.framework 调用 MediaRemote 老式 C API：
 * perl 是 Apple 签名的系统二进制（bundle id 为 com.apple.perl5），mediaremoted 对
 * com.apple. 前缀的客户端放行，因此该机制在 macOS 15.4+（含 Tahoe）依然可用，
 * 且单一机制覆盖所有 macOS 版本。JXA 数据源失效时的全版本兼容兜底。
 */

#[cfg(target_os = "macos")]
const ADAPTER_PL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/vendor/macos/mediaremote-adapter.pl"
));
#[cfg(target_os = "macos")]
const ADAPTER_BIN: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/vendor/macos/MediaRemoteAdapter.framework/Versions/A/MediaRemoteAdapter"
));
#[cfg(target_os = "macos")]
const ADAPTER_INFO_PLIST: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/vendor/macos/MediaRemoteAdapter.framework/Versions/A/Resources/Info.plist"
));
#[cfg(target_os = "macos")]
const ADAPTER_CODE_RESOURCES: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/vendor/macos/MediaRemoteAdapter.framework/Versions/A/_CodeSignature/CodeResources"
));

/// 把内嵌的 perl 脚本与 framework 写出到真实文件系统（框架带符号链接的标准布局）
#[cfg(target_os = "macos")]
fn materialize_vendor() -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join("nowplaying-rs-vendor");
    let fw = root.join("MediaRemoteAdapter.framework");
    let a = fw.join("Versions/A");
    fs::create_dir_all(a.join("Resources")).map_err(|e| format!("创建 vendor 目录失败：{e}"))?;
    fs::create_dir_all(a.join("_CodeSignature")).map_err(|e| format!("创建 vendor 目录失败：{e}"))?;
    let write = |path: &PathBuf, data: &[u8], exe: bool| -> Result<(), String> {
        fs::write(path, data).map_err(|e| format!("写出 {} 失败：{e}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if exe { 0o755 } else { 0o644 };
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|e| format!("设置权限失败：{e}"))?;
        }
        Ok(())
    };
    write(&a.join("MediaRemoteAdapter"), ADAPTER_BIN, true)?;
    write(&a.join("Resources/Info.plist"), ADAPTER_INFO_PLIST, false)?;
    write(
        &a.join("_CodeSignature/CodeResources"),
        ADAPTER_CODE_RESOURCES,
        false,
    )?;
    write(&root.join("mediaremote-adapter.pl"), ADAPTER_PL.as_bytes(), false)?;
    // 标准框架符号链接（旧文件已存在时先删除再重建）
    let link = |src: &str, dst: &str| -> Result<(), String> {
        let dst_path = fw.join(dst);
        let _ = fs::remove_file(&dst_path);
        #[cfg(unix)]
        std::os::unix::fs::symlink(src, &dst_path)
            .map_err(|e| format!("创建符号链接 {} 失败：{e}", dst))?;
        Ok(())
    };
    link("A", "Versions/Current")?;
    link("Versions/Current/MediaRemoteAdapter", "MediaRemoteAdapter")?;
    link("Versions/Current/Resources", "Resources")?;
    Ok(root)
}

/// media-control / mediaremote-adapter stream 的增量更新需要跨行合并状态（diff=true 时 payload 只含变化字段）
pub fn create_adapter_stream_mapper() -> MapLine {
    let mut last: Map<String, Value> = Map::new();
    Box::new(move |obj: &Value| {
        let o = match obj.as_object() {
            Some(o) => o,
            None => return None,
        };
        // 输出形如 {"type":"data","diff":false,"payload":{...}}
        let payload = match o.get("payload").and_then(|p| p.as_object()) {
            Some(p) => p,
            None => o,
        };
        if o.get("diff") == Some(&Value::Bool(false)) {
            last.clear(); // 全量快照：重置状态（停止播放时 payload 为空对象）
        }
        for (k, v) in payload {
            if k == "artworkData" || k == "artworkMIMEType" || v.is_null() {
                continue;
            }
            last.insert(k.clone(), v.clone());
        }
        let title = last
            .get("title")
            .and_then(|t| t.as_str())
            .map(|t| t.trim().to_string())
            .unwrap_or_default();
        if title.is_empty() {
            return None;
        }
        let playing = last.get("playing") == Some(&Value::Bool(true));
        let micros = |v: Option<&Value>| -> Option<f64> {
            match v? {
                Value::Number(n) => n.as_f64().filter(|n| n.is_finite() && *n > 0.0),
                Value::String(s) => s
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite() && *n > 0.0),
                _ => None,
            }
        };
        let bundle = last
            .get("bundleIdentifier")
            .and_then(|b| b.as_str())
            .map(|s| s.to_string());
        let duration_us = micros(last.get("durationMicros"));
        let at_us = micros(last.get("timestampEpochMicros"));
        Some(RawSample {
            title,
            artist: last
                .get("artist")
                .and_then(|a| a.as_str())
                .map(|s| s.to_string()),
            album: last.get("album").and_then(|a| a.as_str()).map(|s| s.to_string()),
            app: bundle.clone(),
            app_id: bundle,
            playing,
            rate: if playing { 1.0 } else { 0.0 },
            position_ms: micros(last.get("elapsedTimeMicros")).unwrap_or(0.0) / 1000.0,
            duration_ms: duration_us.map(|d| d / 1000.0),
            at: at_us.map(|t| (t / 1000.0) as i64).unwrap_or_else(now_ms),
        })
    })
}

/// adapter 为事件驱动（stream 增量推送），不消费轮询间隔；参数仅为与其它工厂签名一致
pub fn create_adapter_provider(interval_ms: u64) -> Box<dyn Provider> {
    let _ = interval_ms;
    #[cfg(target_os = "macos")]
    let precheck: Option<Box<dyn FnOnce() -> Result<(), String> + Send>> =
        Some(Box::new(|| {
            if !std::path::Path::new("/usr/bin/perl").exists() {
                return Err("/usr/bin/perl 不存在".into());
            }
            let vendor = materialize_vendor()?;
            if !vendor.join("mediaremote-adapter.pl").exists() {
                return Err("adapter 脚本缺失".into());
            }
            if !vendor
                .join("MediaRemoteAdapter.framework/MediaRemoteAdapter")
                .exists()
            {
                return Err("framework 缺失".into());
            }
            Ok(())
        }));
    #[cfg(not(target_os = "macos"))]
    let precheck = Some(Box::new(|| -> Result<(), String> {
        Err("非 macOS 平台".into())
    }) as Box<dyn FnOnce() -> Result<(), String> + Send>);

    #[cfg(target_os = "macos")]
    let vendor_dir: String = std::env::temp_dir()
        .join("nowplaying-rs-vendor")
        .to_string_lossy()
        .into_owned();
    #[cfg(not(target_os = "macos"))]
    let vendor_dir: String = String::new();

    Box::new(SpawnLineProvider::new(
        "macos-adapter",
        SpawnLineOptions {
            command: "/usr/bin/perl".into(),
            args: vec![
                format!("{vendor_dir}/mediaremote-adapter.pl"),
                format!("{vendor_dir}/MediaRemoteAdapter.framework"),
                "stream".into(),
                "--no-artwork".into(), // 封面 base64 每次数百 KB，本库不使用，避免无谓的传输与内存开销
                "--micros".into(),
            ],
            probe_timeout_ms: 8_000,
            precheck,
            map_line: Some(create_adapter_stream_mapper()),
            hide_window: false,
        },
    ))
}

