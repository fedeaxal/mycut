//! AI 网关入口：单入口 `dispatch(feature, payload)` + 下载长任务
//! `run_download(kind, payload, emit)` + 同步状态 KV
//! `state_read/state_write/state_delete/state_keys`。
//! 所有 OpenAI 兼容请求为 stream:false 的 JSON POST，阻塞 ureq 每请求
//! 独占线程（无异步运行时依赖）；结果以 JSON 信封回传
//! `{"ok":true,"content":...}` / `{"ok":false,"error":"..."}`。
//! 模板/转写/同音词 FST 的行为契约见各自模块（逐字节移植自渲染端）。

mod download;
mod hr;
mod templates;
mod transcript;

pub mod auth;
pub mod state;

pub use download::{asr_model_status, ensure_asr_model, ensure_web_fonts, list_downloaded_fonts};

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn envelope_ok(content: Value) -> String {
    json!({ "ok": true, "content": content }).to_string()
}

fn envelope_err(error: String) -> String {
    json!({ "ok": false, "error": error }).to_string()
}

/// 输出 token 上限的缺省值（payload 可用 `config.maxTokens` 覆盖）。
///
/// DeepSeek JSON Output 指南要求显式设置 `max_tokens`，否则长 JSON 会在
/// 输出中途被截断。本项目最大的一单是人物表查询（30 人 + 30 个易错词，
/// 约 2–2.5k token），4096 留了一倍余量，也正好是 deepseek-chat 的出厂
/// 默认；模型上限更低的端点会以 400 拒收，[`run_chat`] 去掉可选参数重试。
const DEFAULT_MAX_TOKENS: u64 = 4096;

#[derive(Clone)]
struct RequestConfig {
    base_url: String,
    api_key: String,
    model: String,
    /// 输出 token 上限：payload 的 `config.maxTokens` 可覆盖，缺省
    /// [`DEFAULT_MAX_TOKENS`]。
    max_tokens: u64,
}

/// 与 JS fetch 一致：baseUrl 不做任何清洗（存储层已规范化）。
fn parse_config(payload: &Value) -> Result<RequestConfig, String> {
    let config = &payload["config"];
    Ok(RequestConfig {
        base_url: config["baseUrl"]
            .as_str()
            .ok_or("AI 接口地址未配置")?
            .to_string(),
        api_key: config["apiKey"].as_str().unwrap_or("").to_string(),
        model: config["model"]
            .as_str()
            .filter(|m| !m.is_empty())
            .ok_or("AI 模型未配置")?
            .to_string(),
        max_tokens: config["maxTokens"]
            .as_u64()
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_MAX_TOKENS),
    })
}

fn payload_str<'a>(payload: &'a Value, key: &str) -> &'a str {
    payload[key].as_str().unwrap_or("")
}

fn payload_f64(payload: &Value, key: &str, fallback: f64) -> f64 {
    payload[key].as_f64().unwrap_or(fallback)
}

/// JS `text.slice(0, 200)`（错误详情截断，UTF-16 前缀按字符安全近似）。
fn truncate_200(s: &str) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for ch in s.chars() {
        if units + ch.len_utf16() > 200 {
            break;
        }
        out.push(ch);
        units += ch.len_utf16();
    }
    out
}

/// 单次 /chat/completions POST。非 2xx 的错误文案与 JS fetch 路径一致：
/// `HTTP {status}` 或 `HTTP {status} · {详情前 200 字}`。
fn chat_once(cfg: &RequestConfig, body: &str, timeout_ms: f64) -> Result<String, String> {
    // ureq 3 无默认特性 = 无内置 TLS：显式纯 Rust rustls + 系统证书
    // （与工作区其余 ureq 使用方同一套栈）。4xx/5xx 不作为错误返回，
    // 状态码与错误详情在下方手工判定（信封契约需要详情正文）
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .unversioned_rustls_crypto_provider(provider)
                .build(),
        )
        .http_status_as_error(false)
        .timeout_per_call(Some(Duration::from_millis(timeout_ms.max(1.0) as u64)))
        .build()
        .into();
    let url = format!("{}/chat/completions", cfg.base_url);
    let mut request = agent.post(&url).header("Content-Type", "application/json");
    if !cfg.api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {}", cfg.api_key));
    }
    let response = request.send(body).map_err(|error| match error {
        ureq::Error::Timeout(_) => "请求超时".to_string(),
        other => other.to_string(),
    })?;
    let status = response.status().as_u16();
    let detail = response
        .into_body()
        .read_to_string()
        .unwrap_or_default();
    if !(200..300).contains(&status) {
        let detail = truncate_200(detail.trim());
        if detail.is_empty() {
            return Err(format!("HTTP {status}"));
        }
        return Err(format!("HTTP {status} · {detail}"));
    }
    Ok(detail)
}

/// 取首个 choice 的文本（任何结构异常返回空串，与 JS 收窄语义一致）。
fn first_choice_content(body: &str) -> String {
    let data: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let Some(choices) = data["choices"].as_array() else {
        return String::new();
    };
    let Some(first) = choices.first() else {
        return String::new();
    };
    first["message"]["content"].as_str().unwrap_or("").to_string()
}

/// 组装 body（temperature 缺省时省略字段，与 JS 展开语义一致）。
fn chat_body(
    cfg: &RequestConfig,
    messages: &Value,
    temperature: Option<f64>,
    json_mode: bool,
    max_tokens: Option<u64>,
) -> String {
    let mut body = serde_json::Map::new();
    body.insert("model".into(), json!(cfg.model));
    body.insert("messages".into(), messages.clone());
    body.insert("stream".into(), json!(false));
    if let Some(t) = temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(limit) = max_tokens {
        // DeepSeek JSON Output 指南：显式给足输出预算，否则长 JSON 会被
        // 截断。None 只出现在端点拒收可选参数后的降级重试里。
        body.insert("max_tokens".into(), json!(limit));
    }
    if json_mode {
        // DeepSeek JSON Output 的两个硬性前提之一：response_format 必须是
        // {"type":"json_object"}（另一个是 prompt 含小写 json 字样——
        // 模板已内嵌，templates.rs 的
        // every_json_mode_prompt_contains_json_word 锁定），否则端点 400。
        body.insert("response_format".into(), json!({ "type": "json_object" }));
    }
    Value::Object(body).to_string()
}

/// 组装并发送，返回首个 choice 文本。
///
/// 两个可选参数（`response_format` / `max_tokens`）按端点兼容性兜底：
/// 端点以 HTTP 400/422 拒收（不支持 JSON 模式、或 `max_tokens` 超出模型
/// 上限，如 DeepSeek 推理模型与老端点）→ 去掉这两个参数降级重试一次，
/// 此时非 JSON 输出由消费端宽容解析。JSON 模式下还有第二级兜底（DeepSeek
/// json_mode 指南）：模型输出无法识别为 JSON 时 content 被替换成空格 →
/// 原参数重试一次。
fn run_chat(
    cfg: &RequestConfig,
    messages: Value,
    temperature: Option<f64>,
    timeout_ms: f64,
    json_mode: bool,
) -> Result<String, String> {
    let send = |json_mode: bool, max_tokens: Option<u64>| {
        let body = chat_body(cfg, &messages, temperature, json_mode, max_tokens);
        chat_once(cfg, &body, timeout_ms).map(|raw| first_choice_content(&raw))
    };
    let mut result = send(json_mode, Some(cfg.max_tokens));
    if matches!(
        &result,
        Err(error) if error.starts_with("HTTP 400") || error.starts_with("HTTP 422")
    ) {
        result = send(false, None);
    } else if json_mode && matches!(&result, Ok(text) if text.trim().is_empty()) {
        result = send(true, Some(cfg.max_tokens));
    }
    result
}

/// 带帧请求（chatCompletionWithFrames）：图片只附加在最新一条 user 消息
/// 末尾；带图请求失败（模型不支持/限流/网络）静默回退纯文本重试一次。
fn chat_with_frames(
    cfg: &RequestConfig,
    messages: Value,
    frame_urls: &[String],
    temperature: Option<f64>,
    timeout_ms: f64,
    json_mode: bool,
) -> Result<String, String> {
    let empty = messages
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true);
    if frame_urls.is_empty() || empty {
        return run_chat(cfg, messages, temperature, timeout_ms, json_mode);
    }
    let mut with_frames = messages.clone();
    templates::attach_frames(&mut with_frames, frame_urls);
    match run_chat(cfg, with_frames, temperature, timeout_ms, json_mode) {
        Ok(content) => Ok(content),
        Err(_) => run_chat(cfg, messages, temperature, timeout_ms, json_mode),
    }
}

fn turn_params(payload: &Value) -> templates::TurnParams {
    let params = &payload["params"];
    templates::TurnParams {
        movie_name: params["movieName"].as_str().unwrap_or("").to_string(),
        ai_style: params["aiStyle"].as_str().unwrap_or("").to_string(),
        cast: params["cast"].as_array().cloned().unwrap_or_default(),
        recap: params["recap"]
            .as_str()
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        vision_on: params["visionOn"].as_bool().unwrap_or(false),
        semantic_on: params["semanticOn"].as_bool().unwrap_or(false),
        // 只有实时吐槽 turn 带热词抽取契约；review 等复用方保持旧行为，
        // 由各 dispatch 分支覆写
        hotwords_on: false,
    }
}

fn payload_urls(payload: &Value, key: &str) -> Vec<String> {
    payload[key]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn payload_strings(payload: &Value, key: &str) -> Vec<String> {
    payload[key]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// 整集转写 Map-Reduce：切段优先对白间隙、每段并发低温摘要（每段独立
/// 线程）；全部段失败退化为尾部单段（保底不空手）。
fn map_reduce_summaries(cfg: &RequestConfig, payload: &Value) -> Vec<String> {
    let cues: Vec<Value> = payload["cues"].as_array().cloned().unwrap_or_default();
    let cast = payload["cast"].as_array().cloned().unwrap_or_default();
    let timeout_ms = payload_f64(payload, "timeoutMs", 90_000.0);
    let trimmed = transcript::build_timed_transcript(&cues)
        .trim()
        .to_string();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if transcript::utf16_len(&trimmed) <= templates::TRANSCRIPT_SEGMENT_CHARS {
        return vec![trimmed];
    }
    let segments = transcript::split_transcript_segments(
        &cues,
        templates::TRANSCRIPT_SEGMENT_CHARS,
        5000.0,
    );
    let system = templates::segment_system(&templates::build_cast_context(&cast));
    let total = segments.len();
    let handles: Vec<_> = segments
        .into_iter()
        .enumerate()
        .map(|(index, segment)| {
            let system = system.clone();
            let cfg = cfg.clone();
            let user = templates::segment_user(index + 1, total, &segment);
            std::thread::spawn(move || {
                run_chat(
                    &cfg,
                    json!([
                        { "role": "system", "content": system },
                        { "role": "user", "content": user },
                    ]),
                    Some(0.2),
                    timeout_ms,
                    false,
                )
                .unwrap_or_default()
            })
        })
        .collect();
    let settled: Vec<String> = handles
        .into_iter()
        .filter_map(|handle| handle.join().ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect();
    if settled.is_empty() {
        vec![templates::tail_by_utf16(&trimmed, templates::TRANSCRIPT_SEGMENT_CHARS)]
    } else {
        settled
    }
}

/// 同步执行一个 feature，返回内容值（调用方要信封就用 [`invoke`]）。
///
/// 各 feature 的请求组装+发送（返回首个 choice 文本）。
///
/// json_mode（`response_format={"type":"json_object"}`，见 [`chat_body`]）
/// 只用在回复必须是 JSON 的 feature 上：cast / hotwords / title 恒开，
/// turn 恒开，review 仅在违规筛查开启时开（与提示词里的 JSON 契约同一
/// 条件）。DeepSeek 的硬性要求是这些请求的提示词必须含小写 json 字样，
/// 由 templates.rs 的 `every_json_mode_prompt_contains_json_word` 锁定。
/// 每个请求都显式带 `max_tokens`（缺省 [`DEFAULT_MAX_TOKENS`]）。
pub fn dispatch(feature: &str, payload: &Value) -> Result<Value, String> {
    match feature {
        // 演员表查询（渲染端负责解析与校验）
        "cast" => {
            let cfg = parse_config(payload)?;
            let messages = templates::cast_messages(payload_str(payload, "movieName"));
            let timeout_ms = payload_f64(payload, "timeoutMs", 45_000.0);
            Ok(json!(run_chat(&cfg, messages, None, timeout_ms, true)?))
        }
        // 场景热词查询（创建场景时把 ASR 易错词写入热词.txt；渲染端
        // 负责解析校验与去重落盘）
        "hotwords" => {
            let cfg = parse_config(payload)?;
            let messages = templates::hotwords_messages(payload_str(payload, "movieName"));
            let timeout_ms = payload_f64(payload, "timeoutMs", 45_000.0);
            Ok(json!(run_chat(&cfg, messages, None, timeout_ms, true)?))
        }
        // 实时吐槽一轮：system 模板 + 渲染端 history（append-only）+ 帧突发
        "turn" => {
            let cfg = parse_config(payload)?;
            let mut params = turn_params(payload);
            // turn 恒为 JSON 契约：热词抽取每轮搭车（识别热词实时回流）
            params.hotwords_on = true;
            let system = templates::build_system_prompt(&params);
            let mut messages = json!([{ "role": "system", "content": system }]);
            for turn in payload["history"].as_array().cloned().unwrap_or_default() {
                messages
                    .as_array_mut()
                    .expect("messages 恒为数组")
                    .push(turn);
            }
            let frame_urls = payload_urls(payload, "frameUrls");
            let timeout_ms = payload_f64(payload, "timeoutMs", 60_000.0);
            let temperature = payload["temperature"].as_f64();
            Ok(json!(chat_with_frames(
                &cfg, messages, &frame_urls, temperature, timeout_ms, true
            )?))
        }
        // 整集点评：system 模板 + 要点 basis + 画面轨迹抽样
        "review" => {
            let cfg = parse_config(payload)?;
            let params = turn_params(payload);
            // 仅合规筛查开启时回复才是 JSON 契约（templates.rs
            // build_system_prompt 同款条件），其余为纯文本点评
            let json_mode = params.vision_on || params.semantic_on;
            let system = templates::build_system_prompt(&params);
            let user = templates::review_user(
                payload_str(payload, "basis"),
                payload["framesMeta"].as_array().cloned().unwrap_or_default().as_slice(),
            );
            let frame_urls = payload_urls(payload, "frameUrls");
            let timeout_ms = payload_f64(payload, "timeoutMs", 90_000.0);
            Ok(json!(chat_with_frames(
                &cfg,
                json!([
                    { "role": "system", "content": system },
                    { "role": "user", "content": user },
                ]),
                &frame_urls,
                None,
                timeout_ms,
                json_mode
            )?))
        }
        // 对话回顾压缩
        "compress" => {
            let cfg = parse_config(payload)?;
            let messages = templates::compress_messages(payload_str(payload, "transcript"));
            let timeout_ms = payload_f64(payload, "timeoutMs", 30_000.0);
            Ok(json!(run_chat(&cfg, messages, Some(0.2), timeout_ms, false)?))
        }
        // 标题/简介/标签（渲染端负责 JSON 解析与校验）
        "title" => {
            let cfg = parse_config(payload)?;
            let messages = json!([
                { "role": "system", "content": templates::TITLE_SYSTEM },
                { "role": "user", "content": templates::title_user(
                    payload_str(payload, "movieName"),
                    payload_str(payload, "aiStyle"),
                    &payload_strings(payload, "summaries"),
                    payload["framesMeta"].as_array().cloned().unwrap_or_default().as_slice(),
                ) },
            ]);
            let frame_urls = payload_urls(payload, "frameUrls");
            let timeout_ms = payload_f64(payload, "timeoutMs", 60_000.0);
            Ok(json!(chat_with_frames(&cfg, messages, &frame_urls, Some(0.6), timeout_ms, true)?))
        }
        // 剧情档案滚动（渲染端负责三节结构校验）
        "recap" => {
            let cfg = parse_config(payload)?;
            let messages = templates::recap_messages(
                payload["cast"].as_array().cloned().unwrap_or_default().as_slice(),
                payload["previousContent"].as_str(),
                payload_f64(payload, "previousEpisodes", 0.0),
                &payload_strings(payload, "summaries"),
            );
            let timeout_ms = payload_f64(payload, "timeoutMs", 60_000.0);
            Ok(json!(run_chat(&cfg, messages, Some(0.2), timeout_ms, false)?))
        }
        // Map-Reduce 分段摘要
        "map_reduce" => {
            let cfg = parse_config(payload)?;
            Ok(json!(map_reduce_summaries(&cfg, payload)))
        }
        // 测试连接（最小对话，只验证地址/模型/密钥可达）
        "ping" => {
            let cfg = parse_config(payload)?;
            let body = json!({
                "model": cfg.model,
                "messages": [{ "role": "user", "content": "ping" }],
                "stream": false,
            });
            let timeout_ms = payload_f64(payload, "timeoutMs", 15_000.0);
            chat_once(&cfg, &body.to_string(), timeout_ms)?;
            Ok(json!(null))
        }
        // 同音词规则 + replace.fst（hr:build IPC；网络无关但随网关分发）
        "hr_fst" => {
            let cast = payload["cast"].as_array().cloned().unwrap_or_default();
            let (count, fst) = hr::hr_build(&cast)?;
            Ok(json!({ "count": count, "fst": fst }))
        }
        // 已下载字体清单（渲染端 FontFace 注册用）
        "fonts_list" => {
            let dir = PathBuf::from(payload["dir"].as_str().unwrap_or(""));
            let fonts = download::list_downloaded_fonts(&dir);
            Ok(json!(fonts
                .into_iter()
                .map(|(family, path)| json!({ "family": family, "path": path }))
                .collect::<Vec<_>>()))
        }
        // ASR 识别模型完整性检测（绝不触发下载；启动时只查状态，
        // 下载由用户在 AI 设置框里手动发起）
        "asr_model_status" => {
            let dir = PathBuf::from(payload["dir"].as_str().unwrap_or(""));
            Ok(json!({ "complete": download::asr_model_status(&dir) }))
        }
        // 应用状态 KV（同步、微秒级；主进程直调不走 IPC 事件流）
        "state_keys" => Ok(json!(state::state_keys(
            payload["root"].as_str().unwrap_or("").to_string()
        ))),
        // parity 构建器（无网络）：模板/转写/同音词的逐字节比对通道
        "__parity" => Ok(parity_build(payload)),
        _ => Err(format!("未知 feature: {feature}")),
    }
}

/// __parity 子分发：返回模板/转写/同音词的构建产物（JSON）。
/// 消息类构建通道支持可选 frameUrls 附加（对齐 chatCompletionWithFrames
/// 的装配语义，供 KV 前缀逐字节比对）。
/// ⚠️ serde_json 必须启用 preserve_order：json! 的对象键序要与 JS 对象
/// 字面量一致（默认 BTreeMap 按字母排序会打乱 role/content → KV 前缀
/// 缓存字节序破坏，parity 已实测抓到）。
fn parity_build(payload: &Value) -> Value {
    let attach = |mut messages: Value| -> Value {
        let frame_urls = payload_urls(payload, "frameUrls");
        if !frame_urls.is_empty() {
            templates::attach_frames(&mut messages, &frame_urls);
        }
        messages
    };
    match payload["which"].as_str().unwrap_or("") {
        "clock" => json!(transcript::format_clock_ms(payload["ms"].as_f64().unwrap_or(0.0))),
        "cue_line" => json!(transcript::format_cue_for_ai(&payload["cue"])),
        "segments" => {
            let cues: Vec<Value> = payload["cues"].as_array().cloned().unwrap_or_default();
            json!({
                "transcript": transcript::build_timed_transcript(&cues),
                "segments": transcript::split_transcript_segments(
                    &cues,
                    payload["maxChars"].as_u64().unwrap_or(6000) as usize,
                    payload["gapMs"].as_f64().unwrap_or(5000.0),
                ),
            })
        }
        "cast_context" => json!(templates::build_cast_context(
            payload["cast"].as_array().cloned().unwrap_or_default().as_slice()
        )),
        "system" => json!(templates::build_system_prompt(&turn_params(payload))),
        "segment_messages" => attach(json!([
            { "role": "system", "content": templates::segment_system(payload_str(payload, "castContext")) },
            { "role": "user", "content": templates::segment_user(
                payload["index"].as_u64().unwrap_or(0) as usize,
                payload["total"].as_u64().unwrap_or(0) as usize,
                payload_str(payload, "segment"),
            ) },
        ])),
        "cast_messages" => attach(json!(templates::cast_messages(payload_str(payload, "movieName")))),
        "compress_messages" => attach(json!(templates::compress_messages(payload_str(payload, "transcript")))),
        "title_messages" => attach(json!([
            { "role": "system", "content": templates::TITLE_SYSTEM },
            { "role": "user", "content": templates::title_user(
                payload_str(payload, "movieName"),
                payload_str(payload, "aiStyle"),
                &payload_strings(payload, "summaries"),
                payload["framesMeta"].as_array().cloned().unwrap_or_default().as_slice(),
            ) },
        ])),
        "recap_messages" => attach(templates::recap_messages(
            payload["cast"].as_array().cloned().unwrap_or_default().as_slice(),
            payload["previousContent"].as_str(),
            payload_f64(payload, "previousEpisodes", 0.0),
            &payload_strings(payload, "summaries"),
        )),
        "review_messages" => attach(json!([
            { "role": "system", "content": templates::build_system_prompt(&turn_params(payload)) },
            { "role": "user", "content": templates::review_user(
                payload_str(payload, "basis"),
                payload["framesMeta"].as_array().cloned().unwrap_or_default().as_slice(),
            ) },
        ])),
        "turn_messages" => {
            // 完整组装（含帧附加）供 KV 前缀逐字节比对
            let system = templates::build_system_prompt(&turn_params(payload));
            let mut messages = json!([{ "role": "system", "content": system }]);
            for turn in payload["history"].as_array().cloned().unwrap_or_default() {
                messages
                    .as_array_mut()
                    .expect("messages 恒为数组")
                    .push(turn);
            }
            let frame_urls = payload_urls(payload, "frameUrls");
            if !frame_urls.is_empty() {
                templates::attach_frames(&mut messages, &frame_urls);
            }
            messages
        }
        "hr_rules" => {
            let cast = payload["cast"].as_array().cloned().unwrap_or_default();
            json!(hr::build_hr_rules(&cast)
                .into_iter()
                .map(|(pinyin, target)| json!([pinyin, target]))
                .collect::<Vec<_>>())
        }
        "fst" => {
            let rules: Vec<(String, String)> = payload["rules"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter_map(|pair| {
                    let items = pair.as_array()?;
                    Some((items.first()?.as_str()?.to_string(), items.get(1)?.as_str()?.to_string()))
                })
                .collect();
            match hr::build_replace_fst(&rules) {
                Ok(bytes) => json!({ "ok": true, "base64": hr::base64_encode_public(&bytes) }),
                Err(error) => json!({ "ok": false, "error": error }),
            }
        }
        other => json!({ "ok": false, "error": format!("未知 parity 构建: {other}") }),
    }
}

/// 单入口：每请求独占线程（Map-Reduce 内部再开并发线程后 join）由调用方
/// 决定——本函数同步执行并返回 JSON 信封
/// `{"ok":true,"content":...}` / `{"ok":false,"error":"..."}`；要后台执行
/// 就把它放进自己的线程。panic 被就地接住转信封（信封契约恒成立）。
pub fn invoke(feature: &str, payload: &str) -> String {
    let envelope = std::panic::catch_unwind(|| {
        let parsed: Value = serde_json::from_str(payload)
            .unwrap_or_else(|error| json!({ "__parse_error": error.to_string() }));
        match dispatch(feature, &parsed) {
            Ok(content) => envelope_ok(content),
            Err(error) => envelope_err(error),
        }
    })
    .unwrap_or_else(|_| envelope_err("ai-gateway 内部错误".to_string()));
    envelope
}

/// 下载类 feature（asr_model_ensure / web_fonts_ensure）：长任务，进度
/// 经 `emit` 逐事件上报（status/progress/font_ready），最终事件恒为
/// `{"event":"result","ok":...}`。同步阻塞（调用方放进自己的线程），
/// panic 被就地接住转 result 事件。事件 JSON 形状见 download.rs。
pub fn run_download(kind: &str, payload: &Value, emit: Arc<dyn Fn(Value) + Send + Sync>) {
    let parsed = payload.clone();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match kind {
        "asr_model_ensure" => {
            let dir = PathBuf::from(parsed["dir"].as_str().unwrap_or(""));
            download::ensure_asr_model(&dir, emit.clone())
        }
        "web_fonts_ensure" => {
            let dir = PathBuf::from(parsed["dir"].as_str().unwrap_or(""));
            download::ensure_web_fonts(&dir, &|event| emit(event))
        }
        other => Err(format!("未知下载类型: {other}")),
    }))
    .unwrap_or_else(|_| Err("ai-gateway 下载线程内部错误".to_string()));
    match result {
        Ok(value) => emit(json!({ "event": "result", "ok": true, "value": value })),
        Err(error) => emit(json!({ "event": "result", "ok": false, "error": error })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RequestConfig {
        RequestConfig {
            base_url: "http://localhost".into(),
            api_key: String::new(),
            model: "m".into(),
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }

    #[test]
    fn json_body_carries_response_format_and_token_budget() {
        // DeepSeek JSON Output 的两条硬性要求：response_format 由 json_mode
        // 打开、max_tokens 显式给出（缺省值见 DEFAULT_MAX_TOKENS）。
        let body: Value = serde_json::from_str(&chat_body(
            &config(),
            &json!([{ "role": "user", "content": "json" }]),
            None,
            true,
            Some(DEFAULT_MAX_TOKENS),
        ))
        .unwrap();
        assert_eq!(body["response_format"], json!({ "type": "json_object" }));
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert_eq!(body["stream"], json!(false));

        // 降级重试（参数被端点拒收）不带这两个可选参数，也不带温度以外
        // 的额外字段。
        let body: Value =
            serde_json::from_str(&chat_body(&config(), &json!([]), Some(0.2), false, None))
                .unwrap();
        assert!(body.get("response_format").is_none());
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["temperature"], json!(0.2));
    }

    #[test]
    fn parse_config_defaults_max_tokens() {
        // 没配就用缺省预算；配了（>0）就用配置值。
        let plain = json!({ "config": { "baseUrl": "http://x", "model": "m" } });
        assert_eq!(parse_config(&plain).unwrap().max_tokens, DEFAULT_MAX_TOKENS);
        let budgeted =
            json!({ "config": { "baseUrl": "http://x", "model": "m", "maxTokens": 1024 } });
        assert_eq!(parse_config(&budgeted).unwrap().max_tokens, 1024);
        // 0 视同没配（回到缺省）
        let zero = json!({ "config": { "baseUrl": "http://x", "model": "m", "maxTokens": 0 } });
        assert_eq!(parse_config(&zero).unwrap().max_tokens, DEFAULT_MAX_TOKENS);
    }
}
