//! Reaction 登录门禁的登录标记持久化（防篡改）：
//! - 三值状态：unknown（无标记或任何校验失败）/ ok（已登录）/ no（未登录）。
//! - 防篡改：文件 = 14 字节头部（magic|ver|state|时间戳）XOR 密钥流混淆
//!   （非自描述字节流）+ 32 字节 HMAC-SHA256 完整性标签。密钥由编译期
//!   异或拆分的内置常量与机器指纹（IOPlatformUUID / MachineGuid /
//!   machine-id）派生——本机任何字节改动、或把文件拷到其它机器，读出
//!   都是 unknown（按未持久化处理）。
//! - 写入 tmp+rename 原子替换，进程任意时刻被杀不会损坏旧标记。
//! - 写 true（已登录）只应由主进程的登录轮询成功路径调用；渲染端只能
//!   写 false（未登录），devtools 无法伪造登录态。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::OnceLock;

type HmacSha256 = Hmac<Sha256>;

/// 头部长度：magic(4) + version(1) + state(1) + 时间戳(8, LE)。
const HEADER_LEN: usize = 14;
const TAG_LEN: usize = 32;
const MAGIC: [u8; 4] = *b"OCGA";
const FORMAT_VERSION: u8 = 1;

/// 内置密钥拆成两段异或存放（二进制里不出现密钥本体，运行时组装）。
const SECRET_A: [u8; 32] = [
    0x5a, 0xc3, 0x11, 0x8e, 0xd4, 0x09, 0x76, 0xbb, 0x2f, 0x91, 0xe0, 0x47,
    0x63, 0xda, 0x18, 0xaf, 0xcc, 0x35, 0x8b, 0x52, 0x07, 0xe9, 0x74, 0x1d,
    0xa6, 0x40, 0xd8, 0x0e, 0x9f, 0x23, 0x6c, 0xb1,
];
const SECRET_B: [u8; 32] = [
    0x37, 0xa4, 0x62, 0xbd, 0x90, 0x58, 0x1f, 0xd6, 0x4a, 0xe3, 0x86, 0x20,
    0x15, 0x9c, 0x7b, 0xce, 0xf0, 0x49, 0xaa, 0x33, 0x6e, 0x87, 0x41, 0xd9,
    0xc2, 0x1b, 0x7f, 0x5d, 0xfb, 0x06, 0x39, 0x84,
];

fn embedded_secret() -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = SECRET_A[i] ^ SECRET_B[i];
    }
    out
}

/// 机器指纹：标记文件跨机器拷贝即失效。读取失败回退空串（完整性仍由
/// 内置密钥保障，只是失去机器绑定）。
fn machine_fingerprint() -> &'static str {
    static FINGERPRINT: OnceLock<String> = OnceLock::new();
    FINGERPRINT
        .get_or_init(|| os_machine_id().unwrap_or_default())
        .as_str()
}

#[cfg(target_os = "macos")]
fn os_machine_id() -> Option<String> {
    let output = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        // 形如 `    "IOPlatformUUID" = "XXXX-…"`：等号后的引号段是 UUID
        let Some(rest) = line.strip_prefix("\"IOPlatformUUID\"") else {
            continue;
        };
        if let Some(value) = rest.split('"').nth(1) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn os_machine_id() -> Option<String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("MachineGuid") {
            let value = line.split_whitespace().last()?;
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn os_machine_id() -> Option<String> {
    let raw = std::fs::read_to_string("/etc/machine-id").ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// 密钥材料（OnceLock 缓存）：SHA-256(内置密钥 || 机器指纹)。
fn key_material() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut hasher = Sha256::new();
        hasher.update(embedded_secret());
        hasher.update(machine_fingerprint().as_bytes());
        hasher.finalize().into()
    })
}

/// 混淆密钥流（单块 32 字节 > 头部 14 字节）。
fn keystream(key: &[u8; 32]) -> [u8; HEADER_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update([0x5e]);
    let block: [u8; 32] = hasher.finalize().into();
    let mut out = [0u8; HEADER_LEN];
    out.copy_from_slice(&block[..HEADER_LEN]);
    out
}

fn mark_path(dir: &str) -> Option<PathBuf> {
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join(".cache").join("rtk.dat"))
}

/// 读标记：任何缺失/损坏/校验失败都返回 None（= unknown，按未持久化）。
fn decode(dir: &str) -> Option<bool> {
    let raw = std::fs::read(mark_path(dir)?).ok()?;
    if raw.len() != HEADER_LEN + TAG_LEN {
        return None;
    }
    let (scrambled, tag) = raw.split_at(HEADER_LEN);
    let key = key_material();
    let stream = keystream(key);
    let mut header = [0u8; HEADER_LEN];
    for (i, byte) in header.iter_mut().enumerate() {
        *byte = scrambled[i] ^ stream[i];
    }
    if header[0..4] != MAGIC || header[4] != FORMAT_VERSION {
        return None;
    }
    let mut mac = HmacSha256::new_from_slice(key).ok()?;
    mac.update(&header);
    if mac.verify_slice(tag).is_err() {
        return None;
    }
    match header[5] {
        1 => Some(true),
        0 => Some(false),
        _ => None,
    }
}

/// 写标记：tmp + rename 原子替换。
fn encode(dir: &str, logged_in: bool) -> Result<(), String> {
    let path = mark_path(dir)
        .ok_or_else(|| "非法登录标记存储目录".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut header = [0u8; HEADER_LEN];
    header[0..4].copy_from_slice(&MAGIC);
    header[4] = FORMAT_VERSION;
    header[5] = if logged_in { 1 } else { 0 };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    header[6..HEADER_LEN].copy_from_slice(&ts.to_le_bytes());

    let key = key_material();
    let mut out = [0u8; HEADER_LEN + TAG_LEN];
    let stream = keystream(key);
    for (i, byte) in header.iter().enumerate() {
        out[i] = byte ^ stream[i];
    }
    let mut mac = HmacSha256::new_from_slice(key).map_err(|e| e.to_string())?;
    mac.update(&header);
    out[HEADER_LEN..HEADER_LEN + TAG_LEN].copy_from_slice(&mac.finalize().into_bytes());

    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, out).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// 读取登录标记："unknown"（无标记/损坏/被篡改/跨机器）/ "ok"（已登录）/
/// "no"（未登录）。unknown 不拦截录制——门禁只针对显式的未登录标记。
pub fn auth_state_read(dir: String) -> String {
    match decode(&dir) {
        Some(true) => "ok".to_string(),
        Some(false) => "no".to_string(),
        None => "unknown".to_string(),
    }
}

/// 写入登录标记（tmp+rename 原子）。
pub fn auth_state_write(dir: String, logged_in: bool) -> Result<(), String> {
    encode(&dir, logged_in)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "ai-gw-auth-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn mark_file(dir: &str) -> PathBuf {
        mark_path(dir).unwrap()
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tmp_dir("roundtrip");
        assert_eq!(auth_state_read(dir.clone()), "unknown");
        auth_state_write(dir.clone(), false).unwrap();
        assert_eq!(auth_state_read(dir.clone()), "no");
        auth_state_write(dir.clone(), true).unwrap();
        assert_eq!(auth_state_read(dir), "ok");
    }

    #[test]
    fn any_tampered_byte_reads_unknown() {
        let dir = tmp_dir("tamper");
        auth_state_write(dir.clone(), true).unwrap();
        let path = mark_file(&dir);
        for offset in [0usize, 5, 13, 20, 45] {
            let mut raw = std::fs::read(&path).unwrap();
            raw[offset] ^= 0x01;
            std::fs::write(&path, &raw).unwrap();
            assert_eq!(
                auth_state_read(dir.clone()),
                "unknown",
                "翻转第 {offset} 字节应判为 unknown"
            );
            // 还原后恢复可读
            raw[offset] ^= 0x01;
            std::fs::write(&path, &raw).unwrap();
            assert_eq!(auth_state_read(dir.clone()), "ok");
        }
    }

    #[test]
    fn truncated_or_garbage_reads_unknown() {
        let dir = tmp_dir("junk");
        auth_state_write(dir.clone(), false).unwrap();
        let path = mark_file(&dir);
        let raw = std::fs::read(&path).unwrap();
        std::fs::write(&path, &raw[..raw.len() - 1]).unwrap();
        assert_eq!(auth_state_read(dir.clone()), "unknown");
        std::fs::write(&path, b"garbage").unwrap();
        assert_eq!(auth_state_read(dir.clone()), "unknown");
    }

    #[test]
    fn header_is_scrambled_on_disk() {
        let dir = tmp_dir("scrambled");
        auth_state_write(dir.clone(), true).unwrap();
        let raw = std::fs::read(mark_file(&dir)).unwrap();
        assert!(
            !raw.windows(MAGIC.len()).any(|w| w == MAGIC),
            "盘上不应出现明文 magic"
        );
    }

    #[test]
    fn separate_dirs_are_isolated() {
        let dir_a = tmp_dir("iso-a");
        let dir_b = tmp_dir("iso-b");
        auth_state_write(dir_a.clone(), true).unwrap();
        assert_eq!(auth_state_read(dir_a), "ok");
        assert_eq!(auth_state_read(dir_b), "unknown");
    }
}
