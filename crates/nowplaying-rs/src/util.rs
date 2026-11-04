use std::collections::VecDeque;

use serde_json::Value;

use crate::types::{NowPlayingInfo, RawSample};

/// 当前 epoch 毫秒
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn to_str(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?.trim();
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// 与原版一致：数字直接取，字符串经 Number() 转换，其余为 NaN → None
fn to_num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

/// Windows AUMID（如 `Spotify.exe`、`Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic`）转为友好的应用名
pub fn clean_app_name(raw: &str) -> String {
    let mut s = raw.split('!').next().unwrap_or("").trim();
    if s.len() > 4 && s[s.len() - 4..].eq_ignore_ascii_case(".exe") {
        s = &s[..s.len() - 4];
    }
    // 去掉 UWP 包名尾部的 `_publisherhash`（正则 ^(.*?)_[0-9a-zA-Z]{8,}$：仅末个下划线可能命中）
    if let Some(i) = s.rfind('_') {
        let rest = &s[i + 1..];
        let prefix_ok = i > 0;
        if prefix_ok
            && rest.len() >= 8
            && rest.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

/// 把子进程输出的 JSON 行规整为 RawSample。
/// 返回 None 表示当前没有媒体在播放（心跳行 / 空标题）。
pub fn map_raw_sample(v: &Value) -> Option<RawSample> {
    let obj = v.as_object()?;

    let title = to_str(obj.get("title"))?;

    let now = now_ms();
    let mut at = to_num(obj.get("at")).unwrap_or(0.0) as i64;
    if at <= 0 || at > now + 60_000 {
        at = now; // LastUpdatedTime 可能是未初始化的默认值
    }

    let rate = to_num(obj.get("rate")).unwrap_or(0.0);
    let rate = if rate > 0.0 { rate } else { 0.0 };

    let mut position_ms = to_num(obj.get("positionMs")).unwrap_or(0.0);
    if position_ms < 0.0 {
        position_ms = 0.0;
    }

    let duration_ms = to_num(obj.get("durationMs")).filter(|d| *d > 0.0);

    let app_id = to_str(obj.get("appId")).or_else(|| to_str(obj.get("app")));
    let app = app_id.as_deref().map(clean_app_name);
    Some(RawSample {
        title,
        artist: to_str(obj.get("artist")),
        album: to_str(obj.get("album")),
        app,
        app_id,
        playing: obj.get("playing") == Some(&Value::Bool(true)),
        rate,
        position_ms,
        duration_ms,
        at,
    })
}

/// 由最近一次采样 + 播放速率，实时外推当前进度
pub fn to_info(raw: &RawSample, now: i64) -> NowPlayingInfo {
    let mut position_ms = raw.position_ms;
    if raw.playing && raw.rate > 0.0 {
        position_ms += (now - raw.at).max(0) as f64 * raw.rate;
    }
    if let Some(d) = raw.duration_ms {
        if position_ms > d {
            position_ms = d;
        }
    }
    NowPlayingInfo {
        title: raw.title.clone(),
        artist: raw.artist.clone(),
        album: raw.album.clone(),
        app: raw.app.clone(),
        app_id: raw.app_id.clone(),
        playing: raw.playing,
        position_ms: position_ms.round() as i64,
        duration_ms: raw.duration_ms.map(|d| d.round() as i64),
        updated_at: now,
    }
}

/* ------------------------------------------------------------------ */
/* 假播放识别                                                          */
/* ------------------------------------------------------------------ */

/// 识别「播放器已停止但系统会话滞留」的假播放样本：
/// 会话滞留时系统仍上报 playing/rate，进度或假推进数秒后停滞、或在两个缓存值间振荡。
/// 真播放的进度必然净推进，因此用滑动窗口验证：
///   - 单拍位置跳变 > 5s：视为 seek/换曲，重置窗口（放行）
///   - 换曲：重置窗口（放行）
///   - 窗口时间跨度 ≥ 2.5s 且位置净推进 < 1.5s：判定假播放
#[derive(Debug, Default)]
pub struct StaleFilter {
    last_title: Option<String>,
    window: VecDeque<(f64, i64)>,
}

impl StaleFilter {
    pub fn new() -> Self {
        StaleFilter::default()
    }

    pub fn check(&mut self, s: &RawSample) -> bool {
        if !s.playing || s.rate <= 0.0 {
            self.window.clear();
            self.last_title = Some(s.title.clone());
            return true; // 非播放样本不检测
        }
        if s.position_ms <= 0.0 {
            // 进度未知（如窗口标题数据源），无法用进度验证，直接放行
            self.window.clear();
            self.last_title = Some(s.title.clone());
            return true;
        }
        if self.last_title.as_deref() != Some(s.title.as_str()) {
            self.last_title = Some(s.title.clone());
            self.window.clear();
            return true; // 换曲放行
        }
        if let Some(&(prev_pos, _)) = self.window.back() {
            if (s.position_ms - prev_pos).abs() > 5000.0 {
                self.window.clear(); // seek 等大跳变放行
            }
        }
        self.window.push_back((s.position_ms, s.at));
        if self.window.len() > 5 {
            self.window.pop_front();
        }
        if self.window.len() < 4 {
            return true;
        }
        let (first_pos, first_at) = self.window[0];
        let (last_pos, last_at) = *self.window.back().unwrap();
        let span = (last_at - first_at) as f64 / 1000.0;
        if span < 2.5 {
            return true;
        }
        last_pos - first_pos >= 1500.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample(title: &str, playing: bool, rate: f64, pos: f64, at: i64) -> RawSample {
        RawSample {
            title: title.into(),
            artist: None,
            album: None,
            app: None,
            app_id: None,
            playing,
            rate,
            position_ms: pos,
            duration_ms: None,
            at,
        }
    }

    #[test]
    fn clean_app_name_cases() {
        assert_eq!(clean_app_name("Spotify.exe"), "Spotify");
        assert_eq!(
            clean_app_name("Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic"),
            "Microsoft.ZuneMusic"
        );
        assert_eq!(clean_app_name("PotPlayer"), "PotPlayer");
        assert_eq!(clean_app_name("a_b_12345678"), "a_b");
        assert_eq!(clean_app_name("abc_1234567"), "abc_1234567"); // 尾段不足 8 位不剥
        assert_eq!(clean_app_name("_12345678"), "_12345678"); // 前缀为空不剥
    }

    #[test]
    fn map_raw_sample_normalizes() {
        let now = now_ms();
        let v = json!({
            "title": "  曲目 ", "artist": " ", "at": 0, "rate": -1,
            "positionMs": -5, "durationMs": 0, "appId": "Spotify.exe",
            "playing": true, "extra": 1
        });
        let s = map_raw_sample(&v).unwrap();
        assert_eq!(s.title, "曲目");
        assert_eq!(s.artist, None);
        assert_eq!(s.rate, 0.0);
        assert_eq!(s.position_ms, 0.0);
        assert_eq!(s.duration_ms, None);
        assert_eq!(s.app_id.as_deref(), Some("Spotify.exe"));
        assert_eq!(s.app.as_deref(), Some("Spotify"));
        assert!(s.playing);
        assert!(s.at >= now - 10 && s.at <= now); // 非法 at 回退为当前时刻
    }

    #[test]
    fn map_raw_sample_heartbeat_and_empty_title() {
        assert!(map_raw_sample(&json!({"ok": true, "empty": true})).is_none());
        assert!(map_raw_sample(&json!({"title": "   "})).is_none());
    }

    #[test]
    fn to_info_extrapolates_and_clamps() {
        let base = 1_000_000;
        let s = sample("t", true, 1.0, 10_000.0, base);
        let info = to_info(&s, base + 2_500);
        assert_eq!(info.position_ms, 12_500);
        let s2 = RawSample { duration_ms: Some(12_000.0), ..s.clone() };
        assert_eq!(to_info(&s2, base + 9_000).position_ms, 12_000); // 截到片尾
        let s3 = sample("t", false, 0.0, 7_000.0, base);
        assert_eq!(to_info(&s3, base + 9_000).position_ms, 7_000); // 暂停不外推
    }

    #[test]
    fn stale_filter_detects_frozen_session() {
        let mut f = StaleFilter::new();
        let t0 = 10_000_000;
        // 真播放：净推进 → 全部放行
        for i in 0..6 {
            assert!(f.check(&sample("a", true, 1.0, i as f64 * 1000.0, t0 + i * 1000)));
        }
        // 假播放：时间在走、进度停滞
        let mut rejected = false;
        for i in 6..20 {
            if !f.check(&sample("a", true, 1.0, 5_000.0, t0 + i * 1000)) {
                rejected = true;
                break;
            }
        }
        assert!(rejected, "停滞进度应在窗口跨 2.5s 后被判假");
        // 暂停样本不检测
        assert!(f.check(&sample("a", false, 0.0, 5_000.0, t0 + 30_000)));
        // 换曲放行
        assert!(f.check(&sample("b", true, 1.0, 100.0, t0 + 31_000)));
        assert!(f.check(&sample("b", true, 1.0, 600.0, t0 + 32_000)));
        // seek 大跳变放行
        assert!(f.check(&sample("b", true, 1.0, 90_000.0, t0 + 33_000)));
        assert!(f.check(&sample("b", true, 1.0, 90_400.0, t0 + 34_000)));
    }

    #[test]
    fn to_num_accepts_numeric_strings_only() {
        assert_eq!(to_num(Some(&json!(3.5))), Some(3.5));
        assert_eq!(to_num(Some(&json!("42"))), Some(42.0));
        assert_eq!(to_num(Some(&json!("x"))), None);
        assert_eq!(to_num(Some(&json!(true))), None);
    }
}
