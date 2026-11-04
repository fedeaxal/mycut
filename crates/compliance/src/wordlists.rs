//! 违禁词库（自渲染端 wordlists.ts 移植，词与顺序保持一致）。
//! 命中即 high 风险——词库只收「明确违禁」的词，隐晦表达交给 Layer2
//! 正则规则与 AI 语义层兜底。ASR 错别字导致漏检是关键词法的能力边界。

/// 全局词库（所有平台叠加）。
pub const GLOBAL_WORDS: &[&str] = &[
    // 赌博博彩
    "赌博",
    "赌场",
    "博彩",
    "下注",
    "押注",
    "六合彩",
    // 诈骗灰产
    "刷单",
    "套现",
    "洗钱",
    "诈骗",
    "传销",
    "资金盘",
    "杀猪盘",
    // 违法交易
    "代开发票",
    "办证",
    "私枪",
    "迷药",
    "摇头丸",
    "冰毒",
    "大麻",
    "吸毒",
    "贩毒",
    // 虚假暴富承诺
    "躺赚",
    "躺平赚钱",
    "日入过万",
    "稳赚不赔",
    "保证赚钱",
    "包过",
    // 医疗夸大（prism-agent global_sensitive：特效药/祖传秘方无正则
    // 覆盖，整词进词库；「根治」已由 Layer2 efficacy 正则覆盖，不再
    // 重复收录避免同句双报）
    "特效药",
    "祖传秘方",
    // 违禁品/违规服务
    "代孕",
    "买凶",
    "办假证",
    "开锁教学",
    "翻墙软件",
    "外挂",
    "私服",
    // 极端内容
    "自杀教程",
    "自残",
    "割腕",
    "上吊",
    "安眠药自杀",
];

/// 各平台追加词库（在全局之上叠加）。
const PLATFORM_WORDS: &[(&str, &[&str])] = &[
    (
        "bilibili",
        &[
            // 引流到站外交易（B站社区规则重点）
            "加微信",
            "加V",
            "加QQ",
            "扫码关注公众号",
            "私信转账",
            // 低质营销话术
            "全网最低",
            "史上最低",
            "无效退款",
        ],
    ),
    (
        "douyin",
        &[
            "加微信",
            "加V",
            "加QQ",
            "扫码",
            "私聊",
            "主页联系方式",
            "低价出",
        ],
    ),
    ("kuaishou", &["加微信", "加V", "加QQ", "私聊", "低价出"]),
    (
        "xiaohongshu",
        &["加微信", "加V", "私聊", "扫码", "代购", "拼单"],
    ),
    (
        "wechat-channels",
        &["加微信", "加V", "扫码", "私聊", "保本", "无风险", "稳赚"],
    ),
];

fn platform_extras(platform: &str) -> &'static [&'static str] {
    for (name, words) in PLATFORM_WORDS {
        if *name == platform {
            return words;
        }
    }
    // 未知平台名忽略（渲染端类型约束下不会发生，防御 IPC 边界）
    &[]
}

/// 平台叠加后的去重词表：全局词在前（插入序），随后按平台顺序追加
/// 各平台独有词。与 JS getWordlist 语义一致；空平台列表只用全局词库。
pub fn wordlist_for_platforms(platforms: &[String]) -> Vec<String> {
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut words: Vec<String> = Vec::new();
    let push = |word: &'static str, seen: &mut std::collections::HashSet<&str>, out: &mut Vec<String>| {
        if seen.insert(word) {
            out.push(word.to_string());
        }
    };
    if platforms.is_empty() {
        for word in GLOBAL_WORDS {
            push(word, &mut seen, &mut words);
        }
        return words;
    }
    for platform in platforms {
        for word in GLOBAL_WORDS {
            push(word, &mut seen, &mut words);
        }
        for word in platform_extras(platform) {
            push(word, &mut seen, &mut words);
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(platforms: &[&str]) -> Vec<String> {
        let owned: Vec<String> = platforms.iter().map(|s| s.to_string()).collect();
        wordlist_for_platforms(&owned)
    }

    #[test]
    fn empty_platforms_global_only() {
        let words = list(&[]);
        assert_eq!(words.first().map(String::as_str), Some("赌博"));
        assert_eq!(words.len(), GLOBAL_WORDS.len());
        assert!(!words.iter().any(|w| w == "加微信"));
    }

    #[test]
    fn platforms_overlay_extras_in_order() {
        let words = list(&["kuaishou", "douyin"]);
        // 全局词在前且去重；平台独有词按平台顺序：kuaishou 的"代购"没有，
        // douyin 独有的"扫码"在其后
        assert_eq!(words.first().map(String::as_str), Some("赌博"));
        let first_extra = words.iter().position(|w| w == "加微信").unwrap();
        assert_eq!(first_extra, GLOBAL_WORDS.len());
        assert!(words.iter().any(|w| w == "扫码"));
        assert!(!words.iter().any(|w| w == "代购"));
        assert!(!words.iter().any(|w| w == "拼单"));
    }

    #[test]
    fn duplicate_words_across_platforms_kept_once() {
        let words = list(&["douyin", "kuaishou"]);
        let count = words.iter().filter(|w| *w == "加微信").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn unknown_platform_only_global() {
        let words = list(&["not-a-platform"]);
        assert_eq!(words.len(), GLOBAL_WORDS.len());
    }
}
