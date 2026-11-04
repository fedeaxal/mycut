//! AI prompt 模板（自渲染端 ai-client.ts / ai-commentary.ts 逐字节移植）。
//!
//! ⚠️ 本文件的每个字符串都必须与被替换的 JS 实现逐字节一致：多轮点评的
//! messages 严格 append-only，服务商前缀缓存（KV Cache）按 token 前缀
//! 匹配——任何一个字节的差异都会让整段前缀 miss（命中价约为未命中的
//! 1/10）。parity 脚本对同一批场景状态断言 JS 与 Rust 产物逐字节相等后
//! 才允许切换；改动这里必须先过 parity。

use crate::transcript::{format_clock_ms, utf16_len, utf16_slice};
use serde_json::{json, Value};

pub const SEGMENT_SUMMARY_SYSTEM: &str = "你是影视剧情记录员。把字幕段落压缩成要点：出场人物及其对话/行为、关键剧情事件、情绪转折、高光或金句。保留每条要点开头的时间戳。分点输出，只陈述字幕里出现的内容，不要臆测。";

pub const PLOT_RECAP_SYSTEM: &str = "你是电视剧剧情记录员。把「此前剧情档案」与「本集内容要点」合并成一份更新后的剧情档案，严格按三节输出纯文本（不要 markdown、不要额外解释）：\n【人物现状】主要人物的处境与关系变化，一行一人；\n【剧情进展】按时间顺序的关键事件——旧剧情大幅压缩，本集新事件优先；\n【未解伏笔】尚未交代/待回收的悬念、约定与梗（已被本集解决的要移除）。\n全文 500 字以内。只基于给定材料，不要臆测。";

pub const CAST_SYSTEM: &str =
    "你是影视资料专家，回答只包含 json，不要任何解释或 markdown 之外的文本。";

/// 热词查询 system（fetchHotwordsFromAi；新模板，无 parity 对象）。
pub const HOTWORDS_SYSTEM: &str = "你是语音识别（ASR）数据准备助手。列出剧集中语音识别容易出错或写错的词：角色名、演员名、地名、组织机构、影视/文学作品名、品牌、外语音译、生僻词、网络热词等专有名词。只列确信与该剧相关的词，按出现可能性从高到低排序，最多 30 个，每个不超过 6 个字。回答只包含 json，不要任何解释或 markdown 之外的文本。";

pub const TITLE_SYSTEM: &str =
    "你是短视频运营专家，回答只包含 json，不要任何解释或 markdown 之外的文本。";

pub const COMPRESS_SYSTEM: &str = "你是反应视频的剧情记录员。把观看对话压缩成剧情回顾，严格按三节输出纯文本要点：【出场人物】姓名与关系/印象，一行一人；【剧情进展】按时间顺序的要点；【已点评要点】简短列出（后来者避免重复）。500 字以内，不要客套话。";

/// 点评转写批的分段阈值（ai-client TRANSCRIPT_SEGMENT_CHARS）。
pub const TRANSCRIPT_SEGMENT_CHARS: usize = 6000;

/// 人物表上下文（含一句话人设）：多轮对话 system 与 Map 分段摘要共用。
pub fn build_cast_context(cast: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for entry in cast.iter().take(12) {
        let character = entry["character"].as_str().unwrap_or("");
        let actor = entry["actor"].as_str().unwrap_or("");
        // JS entry.profile 为 truthy 才走带 profile 分支（"" 视为无）
        match entry["profile"].as_str().filter(|p| !p.is_empty()) {
            Some(profile) => lines.push(format!("- {character}（{actor} 扮演）：{profile}")),
            None => lines.push(format!("- {character}（{actor} 扮演）")),
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "【人物表】（字幕中的人名已经同音词纠正为下表写法）：\n{}",
        lines.join("\n")
    )
}

/// 实时点评 turn 的 system 模板参数（录制开始后的首个 turn 冻结一次，
/// 之后每轮原样传入——与 JS 的 history[0] 生命周期一致，保证前缀稳定）。
pub struct TurnParams {
    pub movie_name: String,
    pub ai_style: String,
    pub cast: Vec<Value>,
    /// turn=true：回复恒为 JSON 并搭车热词抽取（hotwords 数组）；
    /// review=false：保持旧行为，仅合规筛查开启时要求 JSON。
    pub hotwords_on: bool,
    pub recap: Option<String>,
    pub vision_on: bool,
    pub semantic_on: bool,
}

/// 实时点评的 system prompt（ai-commentary buildSystemPrompt）。
pub fn build_system_prompt(params: &TurnParams) -> String {
    let cast_context = build_cast_context(&params.cast);
    let recap = params.recap.as_deref().map(str::trim).unwrap_or("");

    // 搭车块（场景级开关，undefined 视为开）。turn（hotwords_on=true）
    // 恒为 JSON 契约：热词抽取每轮搭车（画面/字幕里的 ASR 易错词实时
    // 回流热词.txt，经 asr:update-hotwords 生效），violations 仅合规
    // 筛查开启时携带。review（hotwords_on=false）保持旧行为：仅合规
    // 开启时要求 JSON。
    let compliance_on = params.vision_on || params.semantic_on;
    let compliance_block = if params.hotwords_on {
        let mut block = vec![
            "同时你是语音识别校对助手：根据本批字幕与截图，发现字幕里疑似识别写错的词（同音/近音错字的人名、地名、组织、影视/文学作品名、品牌、外语音译等专有名词，正确写法以画面文字与上下文为准），把正确写法逐条作为hotwords热词返回；没有就返回空数组。热词为简短词组（不超过 6 个字），不要整句。hotwords里只放正确写法本身；commentary是给观众看的点评，字幕纠错说明/对照解释一律不写进commentary，平淡时只写「无」。".to_string(),
        ];
        if params.vision_on {
            block.push("画面检查：结合本批截图检查低俗暴露、血腥暴力、危险行为、违禁品、敏感标志、烟草酒类等画面风险。".to_string());
        }
        if params.semantic_on {
            block.push("语义检查：检查字幕里绕过关键词的隐晦违规（夸大宣传、虚假承诺、引流话术、违禁内容变体等）。".to_string());
        }
        let violations_part = if compliance_on {
            r#","violations":[{"severity":"high或medium","matched":"原句片段或画面概述","reason":"一句话风险说明","fix":"一句话整改建议"}]"#
        } else {
            ""
        };
        block.push(
            format!(
                r#"回复必须是严格的 json（不要 markdown 围栏、不要解释）：{{"commentary":"<点评正文，平淡时为「无」，不要写字幕纠错说明>","hotwords":["<识别写错词的正确写法，没有则空数组>"]{violations_part}}}"#
            ),
        );
        Some(block.join("\n"))
    } else if compliance_on {
        let mut block = vec![
            "同时你是短视频平台的内容审核助手：除点评外，还要在本批内容里筛查发布风险，只报高置信度的问题，宁缺毋滥，没有风险时 violations 返回空数组。".to_string(),
        ];
        if params.vision_on {
            block.push("画面检查：结合本批截图检查低俗暴露、血腥暴力、危险行为、违禁品、敏感标志、烟草酒类等画面风险。".to_string());
        }
        if params.semantic_on {
            block.push("语义检查：检查字幕里绕过关键词的隐晦违规（夸大宣传、虚假承诺、引流话术、违禁内容变体等）。".to_string());
        }
        block.push(
            r#"回复必须是严格的 json（不要 markdown 围栏、不要解释）：{"commentary":"<点评正文，平淡时为「无」>","violations":[{"severity":"high或medium","matched":"原句片段或画面概述","reason":"一句话风险说明","fix":"一句话整改建议"}]}"#.to_string(),
        );
        Some(block.join("\n"))
    } else {
        None
    };

    let movie = if params.movie_name.is_empty() {
        "一部影视作品"
    } else {
        &params.movie_name
    };
    let style = if params.ai_style.trim().is_empty() {
        "说话风格为轻松真诚。"
    } else {
        &params.ai_style
    };

    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "你正在陪用户一起观看《{movie}》并做reaction反应视频。"
    ));
    parts.push(format!("{style}"));
    if !cast_context.is_empty() {
        parts.push(format!(
            "{cast_context}\n表中之外的人名按同音/近音就近对应到表中角色，不要新造人物；「他/她/那个人」等代称结合上下文指代表中角色。"
        ));
    }
    if !recap.is_empty() {
        parts.push(format!(
            "【前情回顾】（截至上一集的剧情档案，仅供理解人物关系与背景）：\n{recap}\n前情仅供参考：前情与本集字幕冲突时以本集字幕为准；前情里没有的剧情不要拿来断言本集；仅当本集内容呼应到前情（伏笔回收/老梗/角色回归）时才提及它。"
        ));
    }
    parts.push("大多数剧集片段其实很平淡：日常对话、过场、寒暄、缓慢推进都不是点评的时机。只有出现笑点、冲突、反转、高能画面或关键剧情时才点评；剧情平淡或没有值得说的点时，commentary 只写一个「无」字，不要硬找话说——空结果是对的，不是失误。".to_string());
    parts.push("用户会持续发来当前听到的字幕：行首 [分:秒] 是录制内时间，部分行带「（情感·事件）」前缀（视频声音的辅助标注，可能有误）。批末可能附带当时的画面截图（文本里逐张标注了时间）——结合字幕、声音标注与画面判断剧情，画面与字幕冲突时以画面为准；除非字幕或画面明确支持，不要断言发生了什么。".to_string());
    parts.push(
        "具体剧情时带上它发生的时间（如 [3:24] 的那句），方便对照；不确定的剧情宁可不提。"
            .to_string(),
    );
    parts.push("请基于最近内容给出接下来可以发散联想的剧外知识/感想/对人物的点评等，可以给用户情绪价值并引起情绪共鸣，直接给可念出来的话，不要客套。".to_string());
    // parts.push("每次输出 1-2 段、每段不超过 40 字，分点用「·」开头。".to_string());
    if let Some(block) = compliance_block {
        parts.push(block);
    }

    parts.join("\n")
}

/// Map 分段摘要的 system（castContext 注入人物表先验）。
pub fn segment_system(cast_context: &str) -> String {
    if cast_context.is_empty() {
        SEGMENT_SUMMARY_SYSTEM.to_string()
    } else {
        format!(
            "{SEGMENT_SUMMARY_SYSTEM}\n\n{cast_context}\n表中之外的同音人名就近对应到表中角色，不要新造人物。"
        )
    }
}

/// Map 分段摘要的 user。
pub fn segment_user(index: usize, total: usize, segment: &str) -> String {
    format!("（整集字幕第 {index}/{total} 段）\n\n{segment}")
}

/// 画面轨迹索引行（标题模板用：「时间为录制内时间」）。
/// ⚠️ 首元素是空串（JS 源如此）：模板里 `\n${frameLines.join("\n")}`
/// 借它产生要点与画面索引之间的空行——parity 实测抓到过漏掉它。
pub fn title_frame_lines(frames_meta: &[Value]) -> Vec<String> {
    if frames_meta.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        String::new(),
        format!(
            "以下是录制中按时间均匀抽样的 {} 张画面截图（按顺序排列，时间为录制内时间），供回看全场画面：",
            frames_meta.len()
        ),
    ];
    for (index, frame) in frames_meta.iter().enumerate() {
        let t_ms = frame["tMs"].as_f64().unwrap_or(0.0);
        lines.push(format!("· 第 {} 张 @ {}", index + 1, format_clock_ms(t_ms)));
    }
    lines
}

/// 标题/简介/标签的 user 模板（ai-client generateRecordingMetadata）。
pub fn title_user(
    movie_name: &str,
    ai_style: &str,
    summaries: &[String],
    frames_meta: &[Value],
) -> String {
    let frame_lines = title_frame_lines(frames_meta);
    let has_frames = !frames_meta.is_empty();
    let movie = if movie_name.is_empty() {
        "影视内容"
    } else {
        movie_name
    };
    let style = if ai_style.is_empty() {
        // 标题/简介/标签的缺省风格与实时吐槽（轻松真诚）不同——
        // 见下方测试 title_user_frames_toggle 的契约
        "轻松点评"
    } else {
        ai_style
    };
    format!(
        "这是一期观看了《{movie}》的反应视频的整集内容要点（按字幕分段整理，自动识别可能有错别字）{}。请为这期视频起一个吸睛的标题、写一段简介、并给出适合的标签。\n整集内容要点：\n{}\n{}\n\n风格要求：{style}。\n\n返回格式：{{\"title\":\"不超过 30 字的标题\",\"description\":\"两三句简介\",\"tags\":[\"标签1\",\"标签2\",\"标签3\",\"标签4\",\"标签5\"]}}",
        if has_frames { "和代表性画面截图" } else { "" },
        summaries.join("\n\n"),
        frame_lines.join("\n"),
    )
}

/// 剧情档案滚动的 messages（updatePlotRecap）。
pub fn recap_messages(
    cast: &[Value],
    previous_content: Option<&str>,
    previous_episodes: f64,
    summaries: &[String],
) -> Value {
    let cast_context = build_cast_context(cast);
    let system = if cast_context.is_empty() {
        PLOT_RECAP_SYSTEM.to_string()
    } else {
        format!("{PLOT_RECAP_SYSTEM}\n\n{cast_context}\n人物名一律用表中写法。")
    };
    let user = [
        match previous_content {
            Some(content) if !content.is_empty() => format!(
                "【此前剧情档案（已看至第 {} 集）】\n{}",
                previous_episodes, content
            ),
            _ => "这是该剧第一集，没有此前档案。".to_string(),
        },
        format!(
            "【本集（第 {} 集）内容要点】\n{}",
            previous_episodes + 1.0,
            summaries.join("\n\n")
        ),
    ]
    .join("\n\n");
    json!([
        { "role": "system", "content": system },
        { "role": "user", "content": user },
    ])
}

/// 演员表查询的 messages（fetchCastFromAi）。
pub fn cast_messages(movie_name: &str) -> Value {
    json!([
        { "role": "system", "content": CAST_SYSTEM },
        { "role": "user", "content": format!(
            "电影/电视剧/综艺《{movie_name}》的主要演员及其在剧中的角色名是什么？列出最多 18 条。每个角色附一句话的人设/人物关系简介（profile）。同时给出每个角色名的普通话拼音（小写字母+数字标调，多音节连写无分隔，例如角色\"玄戒\"写 xuan2jie4，轻声用 1）。\n返回格式：{{\"cast\":[{{\"actor\":\"演员名\",\"character\":\"角色名\",\"pinyin\":\"jiao3se4ming2\",\"profile\":\"一句话人设与人物关系\"}}]}}"
        ) },
    ])
}

/// 对话回顾压缩的 messages（compressHistory）。
pub fn compress_messages(transcript: &str) -> Value {
    json!([
        { "role": "system", "content": COMPRESS_SYSTEM },
        { "role": "user", "content": transcript },
    ])
}

/// 创建场景时的热词查询 messages（渲染端 fetchHotwordsFromAi）。
/// 人名/地名/物名等 ASR 易错词清单，写入场景的 热词.txt 供识别热词偏置。
pub fn hotwords_messages(movie_name: &str) -> Value {
    json!([
        { "role": "system", "content": HOTWORDS_SYSTEM },
        { "role": "user", "content": format!("剧集：《{movie_name}》") },
    ])
}

/// 整集点评的 user 文本（ai-commentary buildReviewRequestText）。
pub fn review_user(basis: &str, frames_meta: &[Value]) -> String {
    let frame_lines: Vec<String> = if frames_meta.is_empty() {
        Vec::new()
    } else {
        // 首元素空串与 JS 源一致（借 join 产生空行分隔）
        let mut lines = vec![
            String::new(),
            format!(
                "以下是录制中按时间均匀抽样的 {} 张画面截图（按顺序排列，时间录制内有效），供回看全场画面：",
                frames_meta.len()
            ),
        ];
        for (index, frame) in frames_meta.iter().enumerate() {
            let t_ms = frame["tMs"].as_f64().unwrap_or(0.0);
            lines.push(format!("· 第 {} 张 @ {}", index + 1, format_clock_ms(t_ms)));
        }
        lines
    };
    [
        format!(
            "以下是这一集的完整字幕记录（按段落整理的要点，自动识别可能有错误）{}。请对整集剧情做一次全面点评：剧情走向、高光片段、人物表现、以及值得做进标题的亮点。分点输出，每点一行、开头标注它依据的时间（如 [12:34]）。点评必须基于记录内容，没有出现的内容不要臆测。",
            if frames_meta.is_empty() { "" } else { "和代表性画面截图" }
        ),
        String::new(),
        basis.to_string(),
    ]
    .into_iter()
    .chain(frame_lines)
    .collect::<Vec<String>>()
    .join("\n")
}

/// 多模态消息内容 → 纯文本（图片部件降级为占位符）。
pub fn chat_message_text(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(parts) = content.as_array() {
        return parts
            .iter()
            .map(|part| {
                if part["type"] == "text" {
                    part["text"].as_str().unwrap_or("").to_string()
                } else {
                    "[画面]".to_string()
                }
            })
            .collect::<Vec<String>>()
            .join("");
    }
    String::new()
}

/// 把帧图片附加到最新一条 user 消息的 content 末尾
///（chatCompletionWithFrames：文本在前、图在后，历史保持纯文本）。
pub fn attach_frames(messages: &mut Value, frame_urls: &[String]) {
    let array = match messages.as_array_mut() {
        Some(a) if !a.is_empty() => a,
        _ => return,
    };
    let last = array.last_mut().expect("非空已保证");
    let role = last["role"].clone();
    let text = chat_message_text(&last["content"]);
    let mut content = vec![json!({ "type": "text", "text": text })];
    for url in frame_urls {
        content.push(json!({ "type": "image_url", "image_url": { "url": url } }));
    }
    *last = json!({ "role": role, "content": content });
}

/// 转写整体的 UTF-16 截断（map_reduce 全部段失败时的尾部保底）。
pub fn tail_by_utf16(s: &str, max: usize) -> String {
    if utf16_len(s) <= max {
        return s.to_string();
    }
    utf16_slice(s, -(max as isize))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cast_entry(character: &str, actor: &str, profile: Option<&str>) -> Value {
        let mut e = json!({ "character": character, "actor": actor });
        if let Some(p) = profile {
            e["profile"] = json!(p);
        }
        e
    }

    #[test]
    fn cast_context_empty_and_filled() {
        assert_eq!(build_cast_context(&[]), "");
        let cast = vec![cast_entry("林美丽", "张三", Some("女主角"))];
        assert_eq!(
            build_cast_context(&cast),
            "【人物表】（字幕中的人名已经同音词纠正为下表写法）：\n- 林美丽（张三 扮演）：女主角"
        );
        // 空 profile 走无 profile 分支（JS truthiness）
        let cast2 = vec![cast_entry("林美丽", "张三", Some(""))];
        assert!(build_cast_context(&cast2).ends_with("- 林美丽（张三 扮演）"));
    }

    #[test]
    fn system_prompt_off_and_on() {
        fn turn_base() -> TurnParams {
            TurnParams {
                movie_name: String::new(),
                ai_style: String::new(),
                cast: vec![],
                recap: None,
                vision_on: false,
                semantic_on: false,
                hotwords_on: false,
            }
        }
        let off = build_system_prompt(&turn_base());
        assert!(off.starts_with("你正在陪用户一起观看《一部影视作品》并做reaction反应视频。"));
        assert!(off.contains("说话风格为轻松真诚。"));
        assert!(off.ends_with("直接给可念出来的话，不要客套。"));
        assert!(!off.contains("内容审核助手"));
        assert!(!off.contains("\"hotwords\""));

        // review 路径（hotwords_on=false）：仅合规开启时要求 JSON
        let on = build_system_prompt(&TurnParams {
            movie_name: "甄嬛传".into(),
            ai_style: "".into(),
            vision_on: true,
            semantic_on: false,
            ..turn_base()
        });
        assert!(on.contains("《甄嬛传》"));
        assert!(on.contains("画面检查"));
        assert!(!on.contains("语义检查"));
        assert!(on.contains("\"commentary\""));

        // turn 路径（hotwords_on=true）：恒带热词抽取契约；violations
        // 仅在合规筛查开启时出现在 JSON 里
        let turn = build_system_prompt(&TurnParams {
            hotwords_on: true,
            ..turn_base()
        });
        assert!(turn.contains("语音识别校对助手"));
        assert!(turn.contains("\"hotwords\""));
        assert!(!turn.contains("\"violations\""));
        let turn_compliance = build_system_prompt(&TurnParams {
            hotwords_on: true,
            vision_on: true,
            ..turn_base()
        });
        assert!(turn_compliance.contains("\"hotwords\""));
        assert!(turn_compliance.contains("\"violations\""));
    }

    #[test]
    fn hotwords_messages_shape() {
        let messages = hotwords_messages("甄嬛传");
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("语音识别（ASR）数据准备助手"));
        assert_eq!(messages[1]["content"], json!("剧集：《甄嬛传》"));
    }

    #[test]
    fn segment_templates() {
        assert_eq!(segment_system(""), SEGMENT_SUMMARY_SYSTEM);
        assert_eq!(
            segment_system("【人物表】X"),
            format!("{SEGMENT_SUMMARY_SYSTEM}\n\n【人物表】X\n表中之外的同音人名就近对应到表中角色，不要新造人物。")
        );
        assert_eq!(segment_user(2, 5, "内容"), "（整集字幕第 2/5 段）\n\n内容");
    }

    #[test]
    fn title_user_frames_toggle() {
        let no_frames = title_user("剧", "", &["要点".into()], &[]);
        assert!(no_frames.starts_with("这是一期观看了《剧》的反应视频的整集内容要点（按字幕分段整理，自动识别可能有错别字）。"));
        assert!(no_frames.contains("风格要求：轻松点评"));
        let frames = vec![json!({ "tMs": 75_000.0 })];
        let with = title_user("", "毒舌", &["要点".into()], &frames);
        assert!(with.contains("和代表性画面截图"));
        assert!(with.contains("· 第 1 张 @ 1:15"));
        assert!(with.contains("《影视内容》"));
    }

    #[test]
    fn recap_first_and_subsequent() {
        let first = recap_messages(&[], None, 0.0, &["要点".into()]);
        assert!(first[1]["content"]
            .as_str()
            .unwrap()
            .starts_with("这是该剧第一集，没有此前档案。"));
        let later = recap_messages(&[], Some("旧档案"), 2.0, &["新要点".into()]);
        let text = later[1]["content"].as_str().unwrap();
        assert!(text.starts_with("【此前剧情档案（已看至第 2 集）】\n旧档案"));
        assert!(text.contains("【本集（第 3 集）内容要点】\n新要点"));
    }

    #[test]
    fn review_user_shape() {
        let text = review_user("要点", &[json!({ "tMs": 0.0 })]);
        assert!(text.starts_with(
            "以下是这一集的完整字幕记录（按段落整理的要点，自动识别可能有错误）和代表性画面截图。"
        ));
        assert!(text.contains("时间录制内有效"));
    }

    #[test]
    fn frame_attachment_order() {
        let mut messages = json!([
            { "role": "system", "content": "sys" },
            { "role": "user", "content": "批字幕" },
            { "role": "assistant", "content": "无" },
            { "role": "user", "content": "新批" },
        ]);
        attach_frames(&mut messages, &["data:image/png;base64,AAA".into()]);
        let last = &messages.as_array().unwrap()[3];
        let parts = last["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "新批");
        assert_eq!(parts[1]["type"], "image_url");
        // 其余消息不受影响
        assert_eq!(messages.as_array().unwrap()[1]["content"], "批字幕");
    }

    #[test]
    fn tail_truncation_utf16() {
        assert_eq!(tail_by_utf16("abc", 10), "abc");
        assert_eq!(tail_by_utf16("abcdef", 2), "ef");
    }
}
