//! AI 喂文本的纯函数（自渲染端 ai-text.ts 逐字节移植）：
//! 字幕→AI 转写行（时间戳+情感/事件标注）、长转写分段。
//! 长度语义保持 JS 的 UTF-16 code unit（切段阈值/切片按 .length）。

use serde_json::Value;

pub const EMOTION_LABELS: &[(&str, &str)] = &[
    ("HAPPY", "开心"),
    ("SAD", "难过"),
    ("ANGRY", "生气"),
    ("FEARFUL", "害怕"),
    ("DISGUSTED", "厌恶"),
    ("SURPRISED", "惊讶"),
];

pub const EVENT_LABELS: &[(&str, &str)] = &[
    ("BGM", "背景音乐"),
    ("Laughter", "笑声"),
    ("Applause", "掌声"),
    ("Cry", "哭声"),
    ("Sneeze", "喷嚏"),
    ("Breath", "呼吸声"),
    ("Cough", "咳嗽声"),
    ("Sing", "唱歌"),
];

fn label_of(table: &[(&str, &str)], key: &str) -> String {
    for (k, v) in table {
        if *k == key {
            return v.to_string();
        }
    }
    // JS: LABELS[key] ?? key —— 未收录 token 原样保留
    key.to_string()
}

/// JS string.length（UTF-16 code units）。
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// JS String.prototype.slice（UTF-16 下标，支持负起点）。
pub fn utf16_slice(s: &str, start: isize) -> String {
    let total = utf16_len(s);
    let start = if start < 0 {
        total.saturating_sub((-start) as usize)
    } else {
        (start as usize).min(total)
    };
    let mut units = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        if units >= start {
            out.push(ch);
        }
        units += ch.len_utf16();
    }
    out
}

/// 录制内时间 → `m:ss`（超一小时 `h:mm:ss`）。
pub fn format_clock_ms(ms: f64) -> String {
    let total_seconds = (ms.max(0.0) / 1000.0).floor() as i64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn cue_str(cue: &Value, key: &str) -> String {
    cue[key].as_str().unwrap_or("").to_string()
}

fn cue_f64(cue: &Value, key: &str) -> f64 {
    cue[key].as_f64().unwrap_or(0.0)
}

/// 一条 cue → `[m:ss]（情感·事件）文本`。
pub fn format_cue_for_ai(cue: &Value) -> String {
    let mut tags: Vec<String> = Vec::new();
    let emotion = cue_str(cue, "emotion");
    if !emotion.is_empty() {
        tags.push(label_of(EMOTION_LABELS, &emotion));
    }
    let event = cue_str(cue, "event");
    if !event.is_empty() {
        tags.push(label_of(EVENT_LABELS, &event));
    }
    let annotation = if tags.is_empty() {
        String::new()
    } else {
        format!("（{}）", tags.join("·"))
    };
    format!(
        "[{}] {}{}",
        format_clock_ms(cue_f64(cue, "startMs")),
        annotation,
        cue_str(cue, "text")
    )
}

/// 整集字幕全文：时间升序、每行带时间戳。
pub fn build_timed_transcript(cues: &[Value]) -> String {
    let mut sorted: Vec<&Value> = cues.iter().collect();
    // JS sort((a,b)=>a.startMs-b.startMs)：稳定排序
    sorted.sort_by(|a, b| {
        cue_f64(a, "startMs")
            .partial_cmp(&cue_f64(b, "startMs"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let lines: Vec<String> = sorted.iter().map(|c| format_cue_for_ai(c)).collect();
    lines.join("\n")
}

/// 长转写按「对白间隙 + 字符上限」切段（字符上限按 UTF-16 单位计）。
pub fn split_transcript_segments(cues: &[Value], max_chars: usize, gap_ms: f64) -> Vec<String> {
    let mut sorted: Vec<&Value> = cues
        .iter()
        .filter(|cue| !cue_str(cue, "text").trim().is_empty())
        .collect();
    sorted.sort_by(|a, b| {
        cue_f64(a, "startMs")
            .partial_cmp(&cue_f64(b, "startMs"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    for (index, cue) in sorted.iter().enumerate() {
        let line = format_cue_for_ai(cue);
        if !current.is_empty() && utf16_len(&current) + utf16_len(&line) + 1 > max_chars {
            segments.push(std::mem::take(&mut current));
            current = line;
        } else {
            current = if current.is_empty() {
                line
            } else {
                format!("{current}\n{line}")
            };
        }
        if let Some(next) = sorted.get(index + 1) {
            if !current.is_empty() && cue_f64(next, "startMs") - cue_f64(cue, "endMs") >= gap_ms {
                segments.push(std::mem::take(&mut current));
            }
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clock_formats() {
        assert_eq!(format_clock_ms(0.0), "0:00");
        assert_eq!(format_clock_ms(61_500.0), "1:01");
        assert_eq!(format_clock_ms(3_600_000.0 + 125_000.0), "1:02:05");
        assert_eq!(format_clock_ms(-5.0), "0:00");
    }

    #[test]
    fn cue_line_with_labels() {
        let cue = json!({"text": "你好", "startMs": 75_000.0, "emotion": "HAPPY", "event": "Laughter"});
        // JS 模板 `[${time}] ${annotation}${text}`：] 后恒有空格
        assert_eq!(format_cue_for_ai(&cue), "[1:15] （开心·笑声）你好");
        let plain = json!({"text": "你好", "startMs": 75_000.0});
        assert_eq!(format_cue_for_ai(&plain), "[1:15] 你好");
        let unknown = json!({"text": "你好", "startMs": 75_000.0, "emotion": "WEIRD"});
        assert_eq!(format_cue_for_ai(&unknown), "[1:15] （WEIRD）你好");
    }

    #[test]
    fn utf16_semantics() {
        // emoji 是 2 个 UTF-16 单位
        assert_eq!(utf16_len("🎉a"), 3);
        assert_eq!(utf16_slice("a🎉b", -2), "b");
    }

    #[test]
    fn segments_split_on_gap_and_limit() {
        let mk = |i: i64| {
            // 每句 10s 间隔说 1s：句间 9s 间隙 ≥ gapMs → 每句一段
            json!({"text": format!("第{i}句"), "startMs": (i as f64) * 10_000.0, "endMs": (i as f64) * 10_000.0 + 1000.0})
        };
        let cues: Vec<Value> = (0..3).map(mk).collect();
        let segments = split_transcript_segments(&cues, 6000, 5000.0);
        assert_eq!(segments.len(), 3);
        assert!(segments[0].starts_with("[0:00]"));
        // 连续说话（句间 0.1s）→ 一段
        let tight: Vec<Value> = (0..3)
            .map(|i| json!({"text": format!("第{i}句"), "startMs": (i as f64) * 1000.0, "endMs": (i as f64) * 1000.0 + 900.0}))
            .collect();
        assert_eq!(split_transcript_segments(&tight, 6000, 5000.0).len(), 1);
    }

    #[test]
    fn transcript_sorted_by_start() {
        let cues = vec![
            json!({"text": "乙", "startMs": 2000.0, "endMs": 2500.0}),
            json!({"text": "甲", "startMs": 1000.0, "endMs": 1500.0}),
        ];
        assert_eq!(build_timed_transcript(&cues), "[0:01] 甲\n[0:02] 乙");
    }
}
