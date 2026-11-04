//! Layer2 六类风险信号正则规则（自渲染端 rules.ts 移植，源自
//! self-media-compliance-review 的 extract_claims.py：绝对化/功效/价格/
//! 数据/竞品/监管话题），含否定上下文排除——「不能说根治」这类提示语不报。
//!
//! 移植差异说明：
//! - JS `\d` 只匹配 ASCII 数字，Rust `\d` 匹配 Unicode Nd——统一改写为
//!   `[0-9]` 保持语义一致；
//! - `100%(?=有效|…)` 这一处 lookahead 用 fancy-regex 支持；
//! - 否定语境窗口按 JS 原逻辑：取命中位前 10 个字符再去空白做后缀匹配。

use fancy_regex::Regex;
use std::sync::LazyLock;

/// 否定/指令上下文词（命中词前 10 字内出现即跳过——teaching 语境）。
const NEGATION_WORDS: &[&str] = &[
    "不", "并不", "不能", "不会", "不要", "别", "禁止", "避免", "删除", "去掉",
    "慎用", "不能写", "不能说", "违规", "敏感词", "违禁词",
];

/// 命中位置（字符下标）前方是否存在否定/指令语境。
pub fn has_negation_context(text: &str, start: usize) -> bool {
    if start == 0 {
        return false;
    }
    let prefix: String = text
        .chars()
        .skip(start.saturating_sub(10))
        // 与 JS slice(start-10, start) 一致：不含命中位本身
        .take(10.min(start))
        .filter(|c| !c.is_whitespace())
        .collect();
    NEGATION_WORDS.iter().any(|word| prefix.ends_with(word))
}

pub struct RuleDef {
    /// 规则标识（finding.ruleId = `claims:${category}`）。
    pub category: &'static str,
    pub severity: &'static str,
    pub reason: &'static str,
    pub fix: &'static str,
    /// 命中内容含这些子串时升级为 blocker（明确的医疗治疗承诺）。
    pub blocker_keywords: &'static [&'static str],
    pub patterns: &'static [&'static str],
}

const REGULATED_BLOCKER_KEYWORDS: &[&str] = &["治疗", "治愈", "根治"];

static CLAIM_RULES: &[RuleDef] = &[
    RuleDef {
        category: "absolute",
        severity: "high",
        reason: "绝对化用语（广告法禁用）",
        fix: "删除该词或改为「很受欢迎」「口碑不错」等非绝对化表达",
        blocker_keywords: &[],
        patterns: &[
            r"最好",
            r"第一[名品牌个]",
            r"唯一",
            r"全网最低",
            r"100%(?=有效|成功|正品|纯|安全)",
            r"绝对(有效|安全|正品|不含)",
            r"保证(有效|成功|通过|赚钱)",
            r"肯定(有效|赚钱|成功)",
        ],
    },
    RuleDef {
        category: "efficacy",
        severity: "high",
        reason: "功效承诺（无法证实）",
        fix: "改为个人体验描述并标注「个人感受，因人而异」",
        blocker_keywords: &[],
        patterns: &[
            r"(必|一定|肯定|绝对)(能|会|可以)?(瘦|白|美|高|好|有效|有用|成功)",
            r"(根治|治愈|永不复发|彻底消除|完全去除)",
            r"[0-9]+天(内|就)?(瘦|白|祛痘|去皱|淡斑|见效)",
            r"(包过|稳赚|必赚|稳赢|必涨)",
            r"(医生|专家|院士|教授)(推荐|认可|背书|代言)",
            r"医学(护肤|级别|认证|研究)",
            r"(干细胞|基因|抗衰|逆龄).{0,5}(技术|成分|效果|因子)",
            r"保证.{0,5}(能|会|可以).{0,5}(瘦|白|美|好|有效)",
        ],
    },
    RuleDef {
        category: "price",
        severity: "medium",
        reason: "价格/促销声明",
        fix: "确保价格声明与商品链接一致，并标注活动期限和条件",
        blocker_keywords: &[],
        patterns: &[
            r"(到手价|券后价|折后价|只需|只要)[^0-9\n]{0,6}[0-9]+(\.[0-9]+)?",
            r"(原价|市场价|专柜价|吊牌价)[^0-9\n]{0,6}[0-9]+(\.[0-9]+)?",
            r"(限时|限量|仅剩|即将涨价|马上涨价)",
            r"买[0-9]+送[0-9]+",
            r"第二[件半价]+",
            r"(免费送|白送|加赠)",
            r"(全网|史上|历史)最低",
            // —— prism-agent PricePromotionRule 补充（促销话术组合）——
            r"(秒杀|抢购|最低价|史低|白菜价).{0,5}(折|元|￥)",
            r"第[一二三四五六七八九]件.{0,3}(半价|[0-9](\.[0-9])?折)",
            r"(0元|免费领|白嫖)",
            r"买[一二三四五六七八九][^0-9\n]{0,3}送",
        ],
    },
    RuleDef {
        category: "data",
        severity: "medium",
        reason: "无出处数据声明",
        fix: "补充数据来源、统计口径和时间范围，或删除该数据",
        blocker_keywords: &[],
        patterns: &[
            r"(销量|已售|卖出)[^0-9\n]{0,4}[0-9]+[万多亿]?",
            r"(好评率|复购率|回头率)[^0-9\n]{0,4}[0-9]+(\.[0-9]+)?%?",
            r"排名[^0-9\n]{0,4}第?[0-9]+",
            r"[0-9]+[万多亿]?\+?\s*(人|位|个|名).{0,5}(在用|选择|购买|推荐)",
        ],
    },
    RuleDef {
        category: "comparison",
        severity: "high",
        reason: "贬低竞品或无法验证的比较",
        fix: "删除对比表达，改为客观描述",
        blocker_keywords: &[],
        patterns: &[
            r"(比|和).{0,10}(好|强|牛|厉害|便宜|有效)[0-9]*倍",
            r"(别人|其他|别家).{0,5}(没有|做不到|不行|差远了)",
            r"(市面|全网|所有|任何).{0,5}(唯一|独有|仅有|找不到)",
            r"(传统|普通|一般|别的).{0,5}(产品|方法|品牌).{0,5}(不行|没用|伤害|危害)",
        ],
    },
];

/// 监管话题规则（命中即 high，blockerKeywords 升级 blocker）。
static REGULATED_RULES: &[RuleDef] = &[    RuleDef {
        category: "regulated",
        severity: "high",
        reason: "涉及「医疗功效」监管领域",
        fix: "删除该表述；医疗功效类声明须有资质证据，否则全部移除",
        blocker_keywords: REGULATED_BLOCKER_KEYWORDS,
        patterns: &[r"(治疗|治愈|主治|医治).{0,10}(疾病|病症|症状|病人)"],
    },
    RuleDef {
        category: "regulated",
        severity: "high",
        reason: "涉及「减肥功效」监管领域",
        fix: "删除该表述；减肥功效类声明须有资质证据，否则全部移除",
        blocker_keywords: REGULATED_BLOCKER_KEYWORDS,
        patterns: &[r"(减肥|瘦身|减重|减脂|瘦脸|瘦腿|瘦肚子)"],
    },
    RuleDef {
        category: "regulated",
        severity: "high",
        reason: "涉及「化妆品功效」监管领域",
        fix: "删除该表述；化妆品功效类声明须有资质证据，否则全部移除",
        blocker_keywords: REGULATED_BLOCKER_KEYWORDS,
        patterns: &[r"(美白|祛斑|淡斑|去皱|抗皱|紧致|提拉|抗衰)"],
    },
    RuleDef {
        category: "regulated",
        severity: "high",
        reason: "涉及「保健功效」监管领域",
        fix: "删除该表述；保健功效类声明须有资质证据，否则全部移除",
        blocker_keywords: REGULATED_BLOCKER_KEYWORDS,
        patterns: &[r"(降血压|降血糖|降血脂|排毒|通便|养胃|护肝|补肾)"],
    },
    RuleDef {
        category: "regulated",
        severity: "high",
        reason: "涉及「教育效果」监管领域",
        fix: "删除该表述；教育效果类声明须有资质证据，否则全部移除",
        blocker_keywords: REGULATED_BLOCKER_KEYWORDS,
        patterns: &[r"(通过率|录取率|上岸|提分|涨分|高分|满分)"],
    },
    RuleDef {
        category: "regulated",
        severity: "high",
        reason: "涉及「金融承诺」监管领域",
        fix: "删除该表述；金融承诺类声明须有资质证据，否则全部移除",
        blocker_keywords: REGULATED_BLOCKER_KEYWORDS,
        patterns: &[r"(收益率|回报率|年化|稳赚|保本|无风险|翻倍)"],
    },
];

/// 引流联系方式变体（源自 prism-agent XiaohongshuContactRule，提升为
/// 全平台规则——引流变体在所有直播平台都是重点治理对象；prism 的
/// `\d{5,}` 5 位纯数字模式不移植：逐句字幕中价格/年份误报率过高）。
static CONTACT_RULES: &[RuleDef] = &[RuleDef {
    category: "contact",
    severity: "high",
    reason: "引流联系方式变体（平台禁止站外导流）",
    fix: "删除联系方式或改为平台内私信/粉丝群引导",
    blocker_keywords: &[],
    patterns: &[
        r"(?i)v信",
        r"威信",
        r"微.{0,2}号",
        r"(加我|联系我|私信我)",
    ],
}];

/// 正文外链（源自 prism-agent NoExternalLinkRule：prism warning，
/// 对齐到本体系 severity medium；每个链接一条，与 prism 多 url 多条
/// 的语义一致）。
static LINK_RULES: &[RuleDef] = &[RuleDef {
    category: "external_link",
    severity: "medium",
    reason: "内容含外部链接（平台可能屏蔽或限流）",
    fix: "删除链接，改为平台内组件或口播引导",
    blocker_keywords: &[],
    patterns: &[r"https?://[^\s]+"],
}];

/// (规则, 已编译正则) 平铺表，编译一次进程内复用（LazyLock）。
pub struct CompiledRule {
    pub def: &'static RuleDef,
    pub regex: Regex,
}

static COMPILED: LazyLock<Vec<CompiledRule>> = LazyLock::new(|| {
    let mut compiled = Vec::new();
    for def in CLAIM_RULES
        .iter()
        .chain(REGULATED_RULES.iter())
        .chain(CONTACT_RULES.iter())
        .chain(LINK_RULES.iter())
    {
        for pattern in def.patterns {
            let regex = Regex::new(pattern)
                .unwrap_or_else(|error| panic!("规则正则编译失败 {pattern}: {error}"));
            compiled.push(CompiledRule { def, regex });
        }
    }
    compiled
});

pub fn all_rules() -> &'static [CompiledRule] {
    &COMPILED
}

/// emoji 刷屏检测（源自 prism-agent ExcessiveEmojiRule：>20 个即提示
/// 营销号风险）。与模式规则不同，这是计数聚合——整句报一条而非每
/// 个 emoji 一条。字符判定对 prism 的 unicodedata(So/Sm) + ord>0x1F300
/// 做码点近似：覆盖主要 emoji 区段（符号/箭头/象形/补充象形/区域旗帜）。
pub fn emoji_excess(text: &str) -> Option<usize> {
    let count = text
        .chars()
        .filter(|c| {
            let code = *c as u32;
            code >= 0x1F300 || (0x2600..=0x27BF).contains(&code) || (0x2B00..=0x2BFF).contains(&code)
        })
        .count();
    (count > 20).then_some(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negation_context_suppresses() {
        // 「不能说赌博」——命中前缀含否定语境
        let text = "这个词不能说赌博";
        let start = text.chars().position(|c| c == '赌').unwrap();
        assert!(has_negation_context(text, start));
        // 正常陈述不受抑制
        let text2 = "今天我们聊聊赌博的危害";
        let start2 = text2.chars().position(|c| c == '赌').unwrap();
        assert!(!has_negation_context(text2, start2));
    }

    #[test]
    fn negation_at_position_zero_no_prefix() {
        assert!(!has_negation_context("赌博害人", 0));
    }

    #[test]
    fn negation_window_strips_whitespace() {
        // 否定词与命中词之间隔空白仍在 10 字窗口内
        let text = "大家千万不能 说脏话";
        let start = text.chars().position(|c| c == '说').unwrap();
        assert!(has_negation_context(text, start));
    }

    #[test]
    fn all_patterns_compile() {
        assert!(all_rules().len() >= 33);
    }

    #[test]
    fn lookahead_pattern_matches() {
        let rule = all_rules()
            .iter()
            .find(|r| r.regex.as_str().contains("100%"))
            .expect("100% 规则存在");
        let m = rule
            .regex
            .find("这个是100%有效的偏方")
            .unwrap()
            .expect("命中");
        assert_eq!(m.as_str(), "100%");
        // 后缀不在白名单内不命中
        assert!(rule.regex.find("100%免费领").unwrap().is_none());
    }
}
