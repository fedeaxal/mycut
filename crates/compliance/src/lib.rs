//! 录制中违规审核本地扫描器（Layer1 违禁词 + Layer2 六类风险信号）。
//! 对每句 ASR 定稿文本调用一次——句子级负载微秒级，录制主循环零感知。
//!
//! 词库、正则规则、否定语境排除全部编译进本二进制。行为契约与被替换的
//! 渲染端实现（scanner.ts/rules.ts/wordlists.ts）逐条对齐：
//! severity/ruleId/matched/reason/fix 字段与去重、截断语义一致。
//! 自 opencut 的 N-API 原生模块移植为纯 Rust 库。

mod automaton;
mod rules;
mod wordlists;

use automaton::Automaton;
use rules::has_negation_context;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

/// 单句扫描结果上限（一句字幕命中 20 条已无增量信息，防极端刷屏）。
const MAX_FINDINGS_PER_TEXT: usize = 20;

/// 单条违规发现（无 id/source：id 与会话来源由宿主赋值——native 不感知
/// mic/desktop 之分）。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Violation {
    pub severity: String,
    pub rule_id: String,
    pub start_ms: f64,
    pub end_ms: f64,
    pub matched: String,
    pub reason: String,
    pub fix: String,
}

/// 平台组合 → 自动机缓存（录制中词库固定，组合数有限）。
fn automaton_for_platforms(platforms: &[String]) -> Arc<Automaton> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<Automaton>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut sorted: Vec<&str> = platforms.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let key = sorted.join(",");
    let mut guard = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .entry(key)
        .or_insert_with(|| {
            Arc::new(Automaton::build(&wordlists::wordlist_for_platforms(platforms)))
        })
        .clone()
}

/// 扫描一句识别文本，返回发现列表。platforms 为空时仅用全局词库。
///
/// 命中时间按命中字符在句内的位置线性内插（句起点 + 占比 × 句长），
/// 而不是把整句起点当作发现时刻：违禁词常在长句中后段（如 10 秒的句
/// 子说在第 7 秒），句级时间会把标记放到错位数秒的地方。
pub fn scan_text(
    text: String,
    start_ms: f64,
    end_ms: f64,
    platforms: Vec<String>,
) -> Vec<Violation> {
    if text.is_empty() {
        return Vec::new();
    }
    let span_ms = (end_ms - start_ms).max(0.0);
    let total_chars = text.chars().count().max(1) as f64;
    // 命中首字符的句内占比 → 句内毫秒。
    let at = |char_index: usize| start_ms + char_index as f64 / total_chars * span_ms;
    let mut findings: Vec<Violation> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Layer1：违禁词（英文词统一小写比较；中文不受影响；命中同样过
    // 否定语境排除——「不能说赌博」不该报）。与 JS 一致：自动机跑在
    // lowered 上，matched 取原文对应切片。
    let lowered = text.to_lowercase();
    let automaton = automaton_for_platforms(&platforms);
    for (word, start) in automaton.find(&lowered) {
        if !seen.insert(format!("kw:{start}:{word}")) {
            continue;
        }
        if has_negation_context(&lowered, start) {
            continue;
        }
        let matched: String = text.chars().skip(start).take(word.chars().count()).collect();
        let hit_start = at(start);
        let hit_end = at(start + word.chars().count()).max(hit_start);
        findings.push(Violation {
            severity: "high".to_string(),
            rule_id: "keyword".to_string(),
            start_ms: hit_start,
            end_ms: hit_end,
            matched,
            reason: format!("疑似违禁词「{word}」"),
            fix: format!("删除或替换「{word}」，或对该句做静音处理"),
        });
    }

    // Layer2：六类风险信号正则（跑在原文上，含否定语境排除与 blocker 升级）
    for rule in rules::all_rules() {
        for m in rule.regex.find_iter(&text) {
            let m = match m {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.as_str().is_empty() {
                continue;
            }
            let start = text[..m.start()].chars().count();
            if has_negation_context(&text, start) {
                continue;
            }
            if !seen.insert(format!("claims:{}:{start}", rule.def.category)) {
                continue;
            }
            let mut severity = rule.def.severity;
            for keyword in rule.def.blocker_keywords {
                if m.as_str().contains(keyword) {
                    severity = "blocker";
                    break;
                }
            }
            let hit_start = at(start);
            let hit_end = at(start + m.as_str().chars().count()).max(hit_start);
            findings.push(Violation {
                severity: severity.to_string(),
                rule_id: format!("claims:{}", rule.def.category),
                start_ms: hit_start,
                end_ms: hit_end,
                matched: m.as_str().to_string(),
                reason: rule.def.reason.to_string(),
                fix: rule.def.fix.to_string(),
            });
        }
    }

    // Layer2 附加：emoji 刷屏计数（prism-agent ExcessiveEmojiRule——
    // 聚合整句报一条，prism info 对齐到本体系 low）
    if let Some(count) = rules::emoji_excess(&text) {
        if seen.insert("claims:emoji:0".to_string()) {
            findings.push(Violation {
                severity: "low".to_string(),
                rule_id: "claims:emoji".to_string(),
                start_ms,
                end_ms,
                matched: format!("{count} 个 emoji"),
                reason: "emoji 使用过多，可能被判定为营销内容".to_string(),
                fix: "减少 emoji 密度，改为自然表达".to_string(),
            });
        }
    }

    findings.truncate(MAX_FINDINGS_PER_TEXT);
    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platforms(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn rule_ids(violations: &[Violation]) -> Vec<&str> {
        violations.iter().map(|v| v.rule_id.as_str()).collect()
    }

    #[test]
    fn keyword_hit_and_negation_exclusion() {
        let hits = scan_text(
            "今天我们聊聊赌博的危害".into(),
            0.0,
            3000.0,
            platforms(&[]),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule_id, "keyword");
        assert_eq!(hits[0].severity, "high");
        assert_eq!(hits[0].matched, "赌博");
        // 「赌博」在第 7 个字符(索引 6)/ 共 11 字符,句内占比内插:
        // 0 + 6/11 × 3000ms ≈ 1636.4ms 起
        assert!((hits[0].start_ms - 6.0 / 11.0 * 3000.0).abs() < 1.0);
        assert!((hits[0].end_ms - 8.0 / 11.0 * 3000.0).abs() < 1.0);
        assert!(hits[0].reason.contains("赌博"));

        let suppressed = scan_text("不能说赌博这个词".into(), 0.0, 1000.0, platforms(&[]));
        assert!(suppressed.is_empty());
    }

    #[test]
    fn platform_wordlist_extends_hits() {
        let none = scan_text("加微信领取福利".into(), 0.0, 1000.0, platforms(&[]));
        assert!(none.is_empty());
        let douyin = scan_text(
            "加微信领取福利".into(),
            0.0,
            1000.0,
            platforms(&["douyin"]),
        );
        assert_eq!(rule_ids(&douyin), vec!["keyword"]);
        assert_eq!(douyin[0].matched, "加微信");
    }

    #[test]
    fn claim_rules_categories_and_severities() {
        let hits = scan_text(
            "这是全网最好的产品".into(),
            1000.0,
            4000.0,
            platforms(&[]),
        );
        // 「全网最低/最好的」：keyword 层 bilibili 才有全网最低，这里只正则层
        let ids = rule_ids(&hits);
        assert!(ids.contains(&"claims:absolute"), "实际: {ids:?}");
        let absolute = hits.iter().find(|v| v.rule_id == "claims:absolute").unwrap();
        assert_eq!(absolute.severity, "high");
        assert_eq!(absolute.matched, "最好");
    }

    #[test]
    fn regulated_blocker_upgrade() {
        let hits = scan_text(
            "这款药能治疗高血压病症".into(),
            0.0,
            2000.0,
            platforms(&[]),
        );
        let regulated = hits
            .iter()
            .filter(|v| v.rule_id == "claims:regulated")
            .collect::<Vec<_>>();
        assert!(!regulated.is_empty());
        // 「治疗」命中 blocker 升级关键词
        assert!(regulated.iter().any(|v| v.severity == "blocker"));
    }

    #[test]
    fn price_rule_medium() {
        let hits = scan_text("到手价只要99元".into(), 0.0, 1000.0, platforms(&[]));
        let price = hits.iter().find(|v| v.rule_id == "claims:price").unwrap();
        assert_eq!(price.severity, "medium");
    }

    #[test]
    fn findings_capped_at_20() {
        let text = "加微信扫码私聊代购拼单加V加QQ低价出保本无风险稳赚躺赚包过外挂私服代孕买凶办证迷药冰毒大麻吸毒贩毒割腕上吊".to_string();
        let hits = scan_text(text, 0.0, 5000.0, platforms(&["douyin", "xiaohongshu"]));
        assert_eq!(hits.len(), 20);
    }

    #[test]
    fn empty_text_no_findings() {
        assert!(scan_text(String::new(), 0.0, 0.0, platforms(&["douyin"])).is_empty());
    }

    #[test]
    fn hit_time_falls_where_the_word_is_said() {
        // 长句后段的命中不再落在句首:10 秒的句子说在第 8 秒,发现就该
        // 标在第 8 秒(而不是 0)——录制违规标记的位置按它来。
        let hits = scan_text(
            "好的各位观众我们今天继续,这一款限量打折的活动别错过".into(),
            10_000.0,
            20_000.0,
            platforms(&[]),
        );
        let price = hits
            .iter()
            .find(|v| v.rule_id == "claims:price")
            .expect("应命中价格/促销规则");
        // 句 26 字符,「限量」在第 17 字符(索引 16):10s + 16/26 × 10s
        let expected = 10_000.0 + 16.0 / 26.0 * 10_000.0;
        assert!((price.start_ms - expected).abs() < 1.0, "实际 {}", price.start_ms);
        assert!(price.start_ms > 15_000.0, "命中时间必须明显晚于句起点");
    }

    #[test]
    fn lowercase_matches_english_words() {
        // 词库全是中文，此处验证 lowered 扫描路径不 panic 且不影响中文命中
        let hits = scan_text("Hello 赌博 World".into(), 0.0, 1000.0, platforms(&[]));
        assert_eq!(hits.len(), 1);
    }

    // ---- prism-agent 补全规则（特效药/祖传秘方、contact、external_link、
    // price 促销组合、emoji 刷屏）----

    #[test]
    fn prism_medical_words_hit_keyword_layer() {
        for word in ["特效药", "祖传秘方"] {
            let hits = scan_text(format!("这款{word}真的有效"), 0.0, 1000.0, platforms(&[]));
            let kw = hits.iter().find(|v| v.rule_id == "keyword").unwrap();
            assert_eq!(kw.matched, word);
            assert_eq!(kw.severity, "high");
        }
    }

    #[test]
    fn contact_variants_detected() {
        for (text, matched) in [
            ("加我V信领资料", "V信"),
            ("加v信 immediately", "v信"),
            ("这个是我们的威信", "威信"),
            ("私信我发你链接", "私信我"),
            ("有问题联系我", "联系我"),
        ] {
            let hits = scan_text(text.into(), 0.0, 1000.0, platforms(&[]));
            let contact = hits
                .iter()
                .find(|v| v.rule_id == "claims:contact")
                .unwrap_or_else(|| panic!("{text} 应命中 contact，实际 {ids:?}", ids = rule_ids(&hits)));
            assert_eq!(contact.severity, "high");
            assert_eq!(contact.matched, matched);
        }
    }

    #[test]
    fn negation_suppresses_contact() {
        // 「不要私信我」——否定语境排除同样作用于 contact 规则
        let hits = scan_text("千万不要私信我".into(), 0.0, 1000.0, platforms(&[]));
        assert!(hits.iter().all(|v| v.rule_id != "claims:contact"));
    }

    #[test]
    fn external_link_detected_per_url() {
        let hits = scan_text(
            "详情看 https://a.example.com/x 和 http://b.example.cn/y".into(),
            0.0,
            1000.0,
            platforms(&[]),
        );
        let links: Vec<_> = hits
            .iter()
            .filter(|v| v.rule_id == "claims:external_link")
            .collect();
        assert_eq!(links.len(), 2, "每个链接一条");
        assert!(links.iter().all(|v| v.severity == "medium"));
    }

    #[test]
    fn prism_price_patterns() {
        for text in [
            "限时秒杀5折起",
            "第二件半价",
            "第三件8折",
            "现在0元领取",
            "点击免费领试用装",
            "买一送二",
        ] {
            let hits = scan_text(text.into(), 0.0, 1000.0, platforms(&[]));
            assert!(
                hits.iter().any(|v| v.rule_id == "claims:price"),
                "{text} 应命中 claims:price，实际 {ids:?}", ids = rule_ids(&hits)
            );
        }
    }

    #[test]
    fn emoji_flood_aggregates_single_low_finding() {
        let text = "🔥✨💥⭐🎉🎊🎁💖🌟🍀🚀💥🔥✨💥⭐🎉🎊🎁💖🌟🍀🚀💥🔥".to_string();
        let hits = scan_text(text, 0.0, 1000.0, platforms(&[]));
        let emoji: Vec<_> = hits.iter().filter(|v| v.rule_id == "claims:emoji").collect();
        assert_eq!(emoji.len(), 1, "聚合一条");
        assert_eq!(emoji[0].severity, "low");
        // 少量 emoji 不报
        let few = scan_text("不错👍".into(), 0.0, 1000.0, platforms(&[]));
        assert!(few.iter().all(|v| v.rule_id != "claims:emoji"));
    }
}
