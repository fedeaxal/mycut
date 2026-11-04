use serde_json::Value;

use crate::providers::spawn_line::{MapLine, SpawnLineOptions, SpawnLineProvider};
use crate::types::RawSample;
use crate::util::{map_raw_sample, now_ms};

/**
 * 数据源：窗口标题兜底（零前置条件，无需 SMTC）。
 * 大多数桌面播放器的窗口标题形如 "文件名 - 播放器名"（如 "input1.mp4 - PotPlayer"）。
 * 本数据源枚举所有可见窗口标题，解析出播放器与媒体名。局限：拿不到播放进度，
 * 且无法感知暂停状态（始终视为播放中）。
 */

/// 长驻 PowerShell：枚举所有可见窗口的 "进程ID\t标题"，每周期输出一行 JSON 数组
pub const TITLE_SCRIPT: &str = r##"
$ErrorActionPreference = 'SilentlyContinue'
try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false) } catch {}

if (-not ('WinEnum' -as [type])) {
  Add-Type -TypeDefinition @"
using System;
using System.Text;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class WinEnum {
  private delegate bool EnumProc(IntPtr hWnd, IntPtr lParam);
  [DllImport("user32.dll")] private static extern bool EnumWindows(EnumProc cb, IntPtr lParam);
  [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr hWnd);
  [DllImport("user32.dll")] private static extern int GetWindowText(IntPtr hWnd, StringBuilder sb, int nMaxCount);
  [DllImport("user32.dll")] private static extern int GetWindowTextLength(IntPtr hWnd);
  [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint lpdwProcessId);
  public static List<string> VisibleTitles() {
    var list = new List<string>();
    EnumWindows(delegate(IntPtr h, IntPtr lp) {
      if (IsWindowVisible(h)) {
        int len = GetWindowTextLength(h);
        if (len > 0) {
          var sb = new StringBuilder(len + 1);
          GetWindowText(h, sb, sb.Capacity);
          uint pid;
          GetWindowThreadProcessId(h, out pid);
          list.Add(pid + "\t" + sb.ToString());
        }
      }
      return true;
    }, IntPtr.Zero);
    return list;
  }
}
"@
}

while ($true) {
  try {
    ConvertTo-Json -Compress -InputObject @([WinEnum]::VisibleTitles())
  } catch {
    ConvertTo-Json -InputObject @{ ok = $false; err = 'enum-failed: ' + $_.Exception.Message } -Compress
    exit 1
  }
  Start-Sleep -Milliseconds __INTERVAL_MS__
}
"##;

/// 已知播放器的窗口标题模式（捕获组 = 媒体名部分）。等价于原版正则列表，
/// hand-rolled 以避免引入 regex 依赖：
/// - `End`：关键字后必须结束（PotPlayer 额外允许可选版本号与位数后缀）
/// - `Any`：关键字后可跟任意内容
/// - `Bracket`：`[foobar2000]` 特殊形式
#[derive(Clone, Copy, PartialEq)]
enum Tail {
    End,
    Any,
    Pot,
    Bracket,
}

const PLAYERS: &[(&str, &str, Tail)] = &[
    ("PotPlayer", "PotPlayer", Tail::Pot),
    ("VLC media player", "VLC", Tail::End),
    ("mpv", "mpv", Tail::End),
    ("MPC-HC", "MPC-HC", Tail::Any),
    ("MPC-BE", "MPC-BE", Tail::Any),
    ("GOM Player", "GOM Player", Tail::Any),
    ("KMPlayer", "KMPlayer", Tail::Any),
    ("PotPlayerMini", "PotPlayer", Tail::Any),
    ("foobar2000", "foobar2000", Tail::Bracket),
    ("AIMP", "AIMP", Tail::Any),
    ("MusicBee", "MusicBee", Tail::End),
    ("Winamp", "Winamp", Tail::Any),
];

const MEDIA_EXTS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "flv", "wmv", "ts", "m2ts", "webm", "mp3", "flac", "m4a", "wav",
    "ape", "ogg", "opus",
];

fn skip_ws(s: &str) -> &str {
    s.trim_start_matches(char::is_whitespace)
}

/// PotPlayer 关键字后允许的可选尾缀：(?:\s[\d.]+)?(?:\s*\((?:32|64)[-\s]bit\))?$
fn potplayer_tail_ok(mut r: &str) -> bool {
    if let Some(body) = r.strip_prefix(char::is_whitespace) {
        let n = body
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .count();
        if n > 0 {
            r = &body[n..];
        }
    }
    let t = skip_ws(r);
    if let Some(after_paren) = t.strip_prefix('(') {
        for ver in ["32", "64"] {
            if let Some(rest) = after_paren.strip_prefix(ver) {
                // [-\s]：一个连字符或空白（正则 /i 不影响这两类字符）
                let rest = rest
                    .strip_prefix('-')
                    .or_else(|| rest.strip_prefix(char::is_whitespace))
                    .unwrap_or(rest);
                if let Some(r2) = strip_prefix_ci(rest, "bit") {
                    if let Some(r3) = r2.strip_prefix(')') {
                        if r3.is_empty() {
                            return true;
                        }
                    }
                }
            }
        }
    }
    r.is_empty()
}

/// 在 title 中寻找最小分割点 i，使 title[i..] 匹配 `\s*[-–]\s*keyword` + 尾缀规则；
/// 返回捕获组 title[..i]。对应正则 ^(.*?)\s*[-–]\s*KEYWORD...$
fn find_capture<'t>(title: &'t str, keyword: &str, tail: Tail) -> Option<&'t str> {
    for (byte_pos, _) in title.char_indices() {
        let rest = skip_ws(&title[byte_pos..]);
        let matched = match tail {
            Tail::Bracket => rest
                .strip_prefix('[')
                .and_then(|a| strip_prefix_ci(a, keyword))
                .map_or(false, |after| {
                    after.strip_prefix(']').map_or(false, str::is_empty)
                }),
            _ => rest
                .strip_prefix('-')
                .or_else(|| rest.strip_prefix('–'))
                .map(skip_ws)
                .and_then(|kw| strip_prefix_ci(kw, keyword))
                .map_or(false, |after_kw| match tail {
                    Tail::End => after_kw.is_empty(),
                    Tail::Any => true,
                    Tail::Pot => potplayer_tail_ok(after_kw),
                    Tail::Bracket => unreachable!(),
                }),
        };
        if matched {
            return Some(&title[..byte_pos]);
        }
    }
    None
}

/// 大小写不敏感的关键字前缀匹配。关键字均为 ASCII，按字节比较：
/// 命中时 keyword.len() 必为 self 的字符边界，也避免按 str 切多字节字符 panic
fn strip_prefix_ci<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let b = s.as_bytes();
    let k = keyword.as_bytes();
    if b.len() >= k.len() && b[..k.len()].eq_ignore_ascii_case(k) {
        Some(&s[k.len()..])
    } else {
        None
    }
}

/// 清除捕获组尾部的 `\s+[-–]\s*$`（最早起点匹配一次），再 trim
fn clean_capture(s: &str) -> &str {
    for (byte_pos, _) in s.char_indices() {
        let rest = &s[byte_pos..];
        let ws_len: usize = rest
            .chars()
            .take_while(|c| c.is_whitespace())
            .map(char::len_utf8)
            .sum();
        if ws_len == 0 {
            continue;
        }
        let after_ws = &rest[ws_len..];
        let dash_len = if after_ws.starts_with('-') {
            1
        } else if after_ws.starts_with('–') {
            '–'.len_utf8()
        } else {
            continue;
        };
        let after_dash = &after_ws[dash_len..];
        if after_dash.is_empty() || after_dash.chars().all(char::is_whitespace) {
            return s[..byte_pos].trim();
        }
    }
    s.trim()
}

/// 从窗口标题列表解析正在播放的媒体。
/// 多窗口命中时取媒体名信息量最大（最长）的一个；" Artist - Title - PotPlayer" 形式
/// 只剥掉播放器后缀，保留内部的 "Artist - Title"。
pub fn parse_window_titles(v: &Value) -> Option<RawSample> {
    let titles = v.as_array()?;
    let mut best: Option<(&str, &str)> = None; // (app, title)
    for item in titles {
        let item = item.as_str().unwrap_or("");
        // PS 输出 "pid\ttitle"；只取标题部分（\t 之后，无 \t 时为整个字符串）
        let raw = match item.find('\t') {
            Some(i) => &item[i + 1..],
            None => item,
        };
        if raw.is_empty() {
            continue;
        }
        for &(kw, app, tail) in PLAYERS {
            if let Some(m) = find_capture(raw, kw, tail) {
                let cleaned = clean_capture(m);
                if !cleaned.is_empty()
                    && best.map_or(true, |(_, t)| cleaned.len() > t.len())
                {
                    best = Some((app, cleaned));
                }
                break;
            }
        }
    }
    let (app, title) = best?;
    let title = strip_media_ext(title);
    map_raw_sample(&serde_json::json!({
        "title": title,
        "app": app,
        "appId": app,
        "playing": true, // 窗口标题无法区分暂停
        "rate": 1,
        "positionMs": 0,
        "durationMs": 0,
        "at": now_ms(),
    }))
}

/// 剥掉媒体文件扩展名（大小写不敏感，必须位于结尾）
fn strip_media_ext(s: &str) -> &str {
    let b = s.as_bytes();
    for ext in MEDIA_EXTS {
        let n = ext.len() + 1;
        if b.len() >= n {
            let i = b.len() - n; // '.' 的位置（ASCII 字节，前后均为字符边界）
            if b[i] == b'.' && s[i + 1..].eq_ignore_ascii_case(ext) {
                return &s[..i];
            }
        }
    }
    s
}

pub fn window_title_map_line() -> MapLine {
    Box::new(parse_window_titles)
}

pub fn create_window_title_provider(interval_ms: u64) -> SpawnLineProvider {
    let script = TITLE_SCRIPT.replace("__INTERVAL_MS__", &interval_ms.max(200).to_string());
    SpawnLineProvider::new(
        "windows-title",
        SpawnLineOptions {
            command: "powershell.exe".into(),
            args: vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-ExecutionPolicy".into(),
                "Bypass".into(),
                "-EncodedCommand".into(),
                crate::providers::windows::encoded_ps_command(&script),
            ],
            probe_timeout_ms: 12_000,
            precheck: None,
            map_line: Some(window_title_map_line()),
            hide_window: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn titles(list: &[&str]) -> Value {
        json!(list)
    }

    #[test]
    fn parses_basic_player_titles() {
        let s = parse_window_titles(&titles(&["1234\tmovie.mkv - PotPlayer"])).unwrap();
        assert_eq!(s.title, "movie");
        assert_eq!(s.app_id.as_deref(), Some("PotPlayer"));
        assert!(s.playing);
        assert_eq!(s.position_ms, 0.0);
        assert_eq!(s.duration_ms, None);
    }

    #[test]
    fn keeps_inner_artist_title() {
        let s =
            parse_window_titles(&titles(&["1\tArtist - Song - VLC media player"])).unwrap();
        assert_eq!(s.title, "Artist - Song");
        assert_eq!(s.app_id.as_deref(), Some("VLC"));
    }

    #[test]
    fn potplayer_version_and_bitness_suffixes() {
        for t in [
            "1\ta.mp3 - PotPlayer 1.7.5213 (64-bit)",
            "1\ta.mp3 - PotPlayer (32 bit)",
            "1\ta.mp3 - PotPlayer",
            "1\ta.mp3 - PotPlayerMini64",
        ] {
            let s = parse_window_titles(&titles(&[t])).unwrap();
            assert_eq!(s.title, "a", "title: {t}");
            assert_eq!(s.app_id.as_deref(), Some("PotPlayer"));
        }
    }

    #[test]
    fn picks_longest_match_across_windows() {
        let s = parse_window_titles(&titles(&[
            "1\t小 - mpv",
            "2\t一个很长的文件名 - MPC-HC x64",
        ]))
        .unwrap();
        assert_eq!(s.title, "一个很长的文件名");
        assert_eq!(s.app_id.as_deref(), Some("MPC-HC"));
    }

    #[test]
    fn foobar2000_bracket_form() {
        let s = parse_window_titles(&titles(&["1\tTrack 01 [foobar2000]"])).unwrap();
        assert_eq!(s.title, "Track 01");
        assert_eq!(s.app_id.as_deref(), Some("foobar2000"));
    }

    #[test]
    fn non_player_titles_ignored() {
        assert!(parse_window_titles(&titles(&["1\t文档.txt - 记事本"])).is_none());
        assert!(parse_window_titles(&titles(&[])).is_none());
    }
}
