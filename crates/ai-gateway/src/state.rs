//! 应用状态持久化（同步 KV）：主进程各状态文件（窗口尺寸/弹幕悬浮窗
//! 位置/录制导入暂存…）与渲染端状态（场景配置/AI 设置/违规报告…）的
//! 统一落盘。root 下每个 key 一个 JSON 文件，tmp+rename 原子写。
//!
//! 同步导出：主进程在启动窗口创建前就要读窗口尺寸（微秒级小文件，
//! 直接在调用线程完成，不引入异步重排）。渲染端经 preload 的
//! sendSync 一次性引导全部状态进内存缓存，之后 get 走缓存、set 异步
//! 落盘——所有既有同步读取契约（zustand persist 首帧、导出弹窗预填、
//! flush 间隔跨窗口生效）无损保留。

use std::path::PathBuf;

/// key → 安全文件名：白名单字符原样，其余字节按 %XX 转义（大写十六进制）。
/// 动态 key 含 `:` 等 Windows 文件名非法字符（如封面编辑器的
/// `opencut-cover-editor:<场景id>`）；白名单字符编码后不变，因此旧文件
/// 与新编码文件名对纯白名单 key 完全一致（兼容既有数据）。编码结果不
/// 含路径分隔符，天然防穿越。
fn encode_key(key: &str) -> Option<String> {
    if key.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(key.len());
    for byte in key.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    Some(out)
}

/// 文件名 → key（encode_key 的逆运算）。
fn decode_key(name: &str) -> String {
    let bytes = name.as_bytes();
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

fn key_to_file(root: &str, key: &str) -> Option<PathBuf> {
    let encoded = encode_key(key)?;
    Some(PathBuf::from(root).join(format!("{encoded}.json")))
}

pub fn state_read(root: String, key: String) -> Option<String> {
    let path = key_to_file(&root, &key)?;
    std::fs::read_to_string(path).ok()
}

/// 原子写：tmp + rename（进程任意时刻被杀都不会留下半个文件）。
pub fn state_write(root: String, key: String, value: String) -> Result<(), String> {
    let Some(path) = key_to_file(&root, &key) else {
        return Err(format!("非法状态 key: {key}"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, value.as_bytes()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn state_delete(root: String, key: String) {
    if let Some(path) = key_to_file(&root, &key) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn state_keys(root: String) -> Vec<String> {
    let mut keys = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return keys;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(encoded) = name.strip_suffix(".json") {
            keys.push(decode_key(encoded));
        }
    }
    keys.sort();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ai-gw-state-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    #[test]
    fn write_read_delete_roundtrip() {
        let root = tmp_root("roundtrip");
        assert_eq!(state_read(root.clone(), "opencut-demo".into()), None);
        state_write(root.clone(), "opencut-demo".into(), "{\"a\":1}".into()).unwrap();
        assert_eq!(state_read(root.clone(), "opencut-demo".into()), Some("{\"a\":1}".into()));
        assert_eq!(state_keys(root.clone()), vec!["opencut-demo".to_string()]);
        state_delete(root.clone(), "opencut-demo".into());
        assert_eq!(state_read(root, "opencut-demo".into()), None);
    }

    #[test]
    fn write_is_atomic_no_tmp_leftover() {
        let root = tmp_root("atomic");
        state_write(root.clone(), "k".into(), "v1".into()).unwrap();
        state_write(root.clone(), "k".into(), "v2".into()).unwrap();
        assert_eq!(state_read(root.clone(), "k".into()), Some("v2".into()));
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留 tmp: {leftovers:?}");
    }

    #[test]
    fn rejects_empty_and_neutralizes_traversal() {
        let root = tmp_root("traversal");
        assert!(state_write(root.clone(), String::new(), "x".into()).is_err());
        // 穿越 key 不再抛错而是整体转义：文件落在 root 内、不产生路径逃逸
        state_write(root.clone(), "../evil".into(), "x".into()).unwrap();
        assert!(
            std::fs::metadata(std::path::Path::new(&root).join("..%2Fevil.json")).is_ok(),
            "穿越 key 应编码为根内安全文件名"
        );
        assert!(!std::path::Path::new(&root).join("evil.json").exists());
        assert_eq!(state_read(root.clone(), "../evil".into()), Some("x".into()));
        assert_eq!(state_keys(root.clone()), vec!["../evil".to_string()]);
    }

    #[test]
    fn colon_key_roundtrip_and_filename_encoding() {
        let root = tmp_root("colon");
        let key = "opencut-cover-editor:cover-editor-main-scene".to_string();
        state_write(root.clone(), key.clone(), "{\"scene\":1}".into()).unwrap();
        assert_eq!(
            state_read(root.clone(), key.clone()),
            Some("{\"scene\":1}".into())
        );
        // 文件名中冒号被转义（Windows 文件名非法字符）
        assert!(
            std::fs::metadata(std::path::Path::new(&root).join(
                "opencut-cover-editor%3Acover-editor-main-scene.json"
            ))
            .is_ok(),
            "冒号应编码为 %3A"
        );
        assert_eq!(state_keys(root.clone()), vec![key.clone()]);
        state_delete(root.clone(), key);
        assert_eq!(state_keys(root), Vec::<String>::new());
    }

    #[test]
    fn encoded_keys_do_not_collide_with_literal_percent() {
        let root = tmp_root("collide");
        state_write(root.clone(), "a:1".into(), "colon".into()).unwrap();
        state_write(root.clone(), "a%3A1".into(), "literal".into()).unwrap();
        assert_eq!(state_read(root.clone(), "a:1".into()), Some("colon".into()));
        assert_eq!(
            state_read(root.clone(), "a%3A1".into()),
            Some("literal".into())
        );
        assert_eq!(state_keys(root).len(), 2);
    }

    #[test]
    fn whitelist_keys_keep_legacy_filenames() {
        // 纯白名单 key 的编码结果与旧实现文件名一致（兼容既有数据文件）
        assert_eq!(
            encode_key("opencut-reaction-scenes").as_deref(),
            Some("opencut-reaction-scenes")
        );
    }
}
