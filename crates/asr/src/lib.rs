//! ASR 入口：SenseVoice 离线识别（热词实时更新）+ silero VAD。
//!
//! 自 opencut 的 N-API 原生模块移植为纯 Rust 库：napi 的 object/Float32Array
//! 换成普通结构与 `Vec<f32>`，napi Error 换成 `Result<_, String>`，其余行为
//! 契约不变——
//! - `Recognizer::create / create_stream / decode / get_result /
//!   update_hotwords_text`（热词实时更新，AI 提词过程中新增词立即生效）
//! - `Vad::create / accept_waveform / is_empty / front / pop / flush`
//!
//! 模型文件缺失校验在创建前完成：sherpa 对缺失/空模型不抛错，会构造
//! 内部空指针实例，之后每个音频块都在 C 层刷错误——校验前置后缺失走
//! `Err` 一次性降级。
//!
//! 识别/VAD 的实现绑定 sherpa-onnx（构建期拉预编译 C 库），收在
//! `sensevoice` feature 后面；解析与校验等纯逻辑始终可用。

use std::path::Path;

/// 模型文件前置校验（缺失/损坏都以 `Err` 报给调用方降级）。
#[cfg(feature = "sensevoice")]
fn validate_model_file(path: &str, label: &str) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|_| format!("{label} 缺失: {path}"))?;
    if !meta.is_file() || meta.len() < 1024 {
        return Err(format!("{label} 不完整({}B): {path}", meta.len()));
    }
    Ok(())
}

/// SenseVoice 模型全路径（model_dir/model.int8.onnx）。
pub fn sense_voice_model_path(model_dir: &str) -> String {
    Path::new(model_dir)
        .join("model.int8.onnx")
        .to_string_lossy()
        .into_owned()
}

/// 识别器选项（原 napi object）。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct RecognizerOptions {
    /// SenseVoice 模型目录（model.int8.onnx 所在）。
    pub model_dir: String,
    /// tokens.txt 全路径（同目录由宿主拼好传入）。
    pub tokens_path: String,
    /// "greedy_search"（缺省）或 "modified_beam_search"（热词必需）。
    pub decoding_method: Option<String>,
    /// beam 宽度（modified_beam_search 时生效，缺省 8）。
    pub max_active_paths: Option<u32>,
    /// 热词文件全路径（UTF-8，每行一个词，# 注释）。
    pub hotwords_file: Option<String>,
    /// 全局热词分数（缺省 1.5，推荐 1.5-2.0）。
    pub hotwords_score: Option<f64>,
    /// 同音词替换 lexicon（内置 resources/asr/lexicon.txt）。
    pub hr_lexicon: Option<String>,
    /// 同音词替换规则 replace.fst 全路径（场景 hrPath）。
    pub rule_fsts: Option<String>,
    /// 识别语言：SenseVoice 小模型认 "auto"/"zh"/"en"/"ja"/"ko"/"yue"，
    /// 其余 sherpa 会告警并回落 auto。空/缺省 = auto。
    pub language: Option<String>,
    /// ONNX Runtime 线程数（缺省 2）。
    pub num_threads: Option<u32>,
}

/// 单段识别结果（emotion/event 已剥离 `<|...|>` 标签，保留原始大小写，
/// 由调用方按 non-reportable 集合过滤）。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AsrSegment {
    pub text: String,
    pub emotion: Option<String>,
    pub event: Option<String>,
    /// 与正文一一对应的文本 token（不含 lang/emotion/event 特殊 token）。
    pub tokens: Vec<String>,
    /// 各 token 时间（秒，相对段起点），可能缺省。
    pub timestamps: Vec<f64>,
}

/// SenseVoice 结果 JSON（C 端 `ResultAsJson`）→ 结构化结果。special
/// token 是 `<|TAG|>` 形式，剥离尖括号标签后原样透传；正文/text 缺失
/// 或 JSON 损坏返回 None（调用方按"无结果"降级）。
pub fn parse_sense_voice_json(raw: &str) -> Option<AsrSegment> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let text = v.get("text")?.as_str()?.to_string();
    let strip_tag = |key: &str| -> Option<String> {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(|token| {
                token
                    .trim()
                    .trim_start_matches("<|")
                    .trim_end_matches("|>")
                    .trim()
                    .to_string()
            })
            // 剥离后为空（如 `<||>`）按缺省处理
            .filter(|token| !token.is_empty())
    };
    let tokens = v
        .get("tokens")
        .and_then(|x| x.as_array())
        .map(|items| items.iter().filter_map(|t| t.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let timestamps = v
        .get("timestamps")
        .and_then(|x| x.as_array())
        .map(|items| items.iter().filter_map(|t| t.as_f64()).collect())
        .unwrap_or_default();
    Some(AsrSegment {
        text,
        emotion: strip_tag("emotion"),
        event: strip_tag("event"),
        tokens,
        timestamps,
    })
}

/// VAD 选项（原 napi object）。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VadOptions {
    /// silero_vad.onnx 全路径。
    pub model_path: String,
    /// 缺省 16000。
    pub sample_rate: Option<u32>,
    /// 缺省 0.5。
    pub threshold: Option<f64>,
    /// 最短语音段（秒，缺省 0.25）。
    pub min_speech_duration: Option<f64>,
    /// 分段静音间隔（秒，缺省 0.4）。
    pub min_silence_duration: Option<f64>,
    /// 固定窗口长度（样本数，silero 要求 512，缺省 512）。
    pub window_size: Option<u32>,
    /// 最长语音段（秒，缺省 8）。
    pub max_speech_duration: Option<f64>,
    /// 内部结果缓冲（秒，缺省 30）。
    pub buffer_size_in_seconds: Option<f64>,
    /// 缺省 1。
    pub num_threads: Option<u32>,
}

/// 单段语音（原 napi object；samples 为拷贝）。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct VadSegment {
    /// 段起点（样本索引，相对送入 VAD 的全部样本）。
    pub start: f64,
    /// 16kHz Float32 PCM。
    pub samples: Vec<f32>,
}

#[cfg(feature = "sensevoice")]
mod native {
    //! 绑定 sherpa-onnx 的识别器与 VAD（`sensevoice` feature）。

    use super::{validate_model_file, AsrSegment, RecognizerOptions, VadOptions, VadSegment};
    use sherpa_onnx::{
        OfflineRecognizer as NativeRecognizer, OfflineRecognizerConfig,
        OfflineSenseVoiceModelConfig, SileroVadModelConfig, VadModelConfig, VoiceActivityDetector,
    };

    /// CString 构造：文本含内嵌 NUL 时跨 FFI 是未定义行为，这里显式报错
    /// 而非 unwrap panic（热词文本来自 AI 响应/用户文件，不可信）。
    fn c_string(text: &str, label: &str) -> Result<String, String> {
        if text.contains('\0') {
            return Err(format!("{label} 含非法空字符"));
        }
        Ok(text.to_string())
    }

    /// 离线识别器（SenseVoice）。
    pub struct Recognizer {
        inner: NativeRecognizer,
    }

    impl Recognizer {
        /// 创建离线识别器。模型 200MB+，首次加载秒级；同参数下应进程级
        /// 复用实例。
        pub fn create(options: RecognizerOptions) -> Result<Recognizer, String> {
            let model_path = super::sense_voice_model_path(&options.model_dir);
            validate_model_file(&model_path, "SenseVoice 模型")?;
            validate_model_file(&options.tokens_path, "tokens")?;

            let mut config = OfflineRecognizerConfig::default();
            // feat_config 缺省即 16000/80（SenseVoice 固定规格）
            config.model_config.sense_voice = OfflineSenseVoiceModelConfig {
                model: Some(model_path),
                // 语言是识别器创建参数（调用方按会话传：字幕侧用用户所选，
                // AI 侧恒 auto）；空/缺省 = 自动检测
                language: Some(
                    options
                        .language
                        .filter(|language| !language.is_empty())
                        .unwrap_or_else(|| "auto".into()),
                ),
                use_itn: true,
            };
            config.model_config.tokens = Some(options.tokens_path);
            config.model_config.num_threads = options.num_threads.unwrap_or(2) as i32;
            config.model_config.provider = Some("cpu".into());
            config.model_config.debug = false;
            // 热词只在 modified_beam_search 下生效（greedy 时 sherpa 忽略）；
            // 无热词的会话保持 greedy_search，识别速度与旧行为一致
            config.decoding_method = Some(
                options
                    .decoding_method
                    .unwrap_or_else(|| "greedy_search".into()),
            );
            config.max_active_paths = options.max_active_paths.unwrap_or(8) as i32;
            config.hotwords_file = options.hotwords_file;
            config.hotwords_score = options.hotwords_score.unwrap_or(1.5) as f32;
            // 同音词替换与热词是独立机制，可共存（hr 在 C 端对输出 token 序列
            // 做规则替换；热词在 beam 搜索阶段偏置）
            if options.hr_lexicon.is_some() || options.rule_fsts.is_some() {
                config.hr = sherpa_onnx::HomophoneReplacerConfig {
                    lexicon: options.hr_lexicon,
                    rule_fsts: options.rule_fsts,
                };
            }

            let inner = NativeRecognizer::create(&config).ok_or_else(|| {
                "SenseVoice 识别器创建失败（检查模型文件与日志）".to_string()
            })?;
            Ok(Recognizer { inner })
        }

        /// 创建空识别流（每段语音一个，用完即弃）。
        pub fn create_stream(&self) -> AsrStream {
            AsrStream {
                inner: self.inner.create_stream(),
            }
        }

        /// 解码一个流（同步阻塞，本进程内串行执行）。
        pub fn decode(&self, stream: &AsrStream) {
            self.inner.decode(&stream.inner);
        }

        /// 取最近一次 decode 的结果；无文本/JSON 解析失败返回 None。
        pub fn get_result(&self, stream: &AsrStream) -> Option<AsrSegment> {
            let raw = stream.inner.get_sensevoicesmall_result()?;
            super::parse_sense_voice_json(&raw)
        }

        /// 替换后续 decode 使用的热词全文（与热词文件同格式：每行一个词，
        /// # 注释）。仅 modified_beam_search 的 SenseVoice 生效；重建约 1ms，
        /// 可在录制中随 AI 返回的新词实时调用。不得与 decode 并发。
        pub fn update_hotwords_text(&self, text: String) -> Result<(), String> {
            let text = c_string(&text, "热词文本")?;
            self.inner.update_hotwords_text(&text);
            Ok(())
        }
    }

    pub struct AsrStream {
        inner: sherpa_onnx::OfflineStream,
    }

    impl AsrStream {
        /// 追加 PCM 样本（Float32，采样率须与 feat 一致）。
        pub fn accept_waveform(&self, sample_rate: i32, samples: &[f32]) {
            self.inner.accept_waveform(sample_rate, samples);
        }
    }

    /// silero VAD。
    pub struct Vad {
        inner: VoiceActivityDetector,
    }

    impl Vad {
        /// 创建 VAD。文件缺失时 sherpa 不抛错而是构造空指针实例后每块刷
        /// "vad is nullptr"——校验前置。
        pub fn create(options: VadOptions) -> Result<Vad, String> {
            validate_model_file(&options.model_path, "silero_vad.onnx")?;
            let config = VadModelConfig {
                silero_vad: SileroVadModelConfig {
                    model: Some(options.model_path),
                    threshold: options.threshold.unwrap_or(0.5) as f32,
                    min_speech_duration: options.min_speech_duration.unwrap_or(0.25) as f32,
                    min_silence_duration: options.min_silence_duration.unwrap_or(0.4) as f32,
                    window_size: options.window_size.unwrap_or(512) as i32,
                    max_speech_duration: options.max_speech_duration.unwrap_or(8.0) as f32,
                },
                ten_vad: Default::default(),
                sample_rate: options.sample_rate.unwrap_or(16000) as i32,
                num_threads: options.num_threads.unwrap_or(1) as i32,
                provider: Some("cpu".into()),
                debug: false,
            };
            let inner = VoiceActivityDetector::create(
                &config,
                options.buffer_size_in_seconds.unwrap_or(30.0) as f32,
            )
            .ok_or_else(|| "silero VAD 创建失败（检查模型文件与日志）".to_string())?;
            Ok(Vad { inner })
        }

        /// 送入一个固定长度窗口（window_size 样本）。
        pub fn accept_waveform(&self, window: &[f32]) {
            self.inner.accept_waveform(window);
        }

        pub fn is_empty(&self) -> bool {
            self.inner.is_empty()
        }

        /// 取队首语音段（samples 恒为拷贝）。
        pub fn front(&self) -> Option<VadSegment> {
            let native = self.inner.front()?;
            let samples = native.samples().to_vec();
            let start = native.start();
            drop(native);
            Some(VadSegment {
                start: start as f64,
                samples,
            })
        }

        /// 弹出队首语音段（与 front 成对调用）。
        pub fn pop(&self) {
            self.inner.pop();
        }

        /// 把缓冲中的尾段语音压入输出队列（会话结束前调用）。
        pub fn flush(&self) {
            self.inner.flush();
        }
    }
}

#[cfg(feature = "sensevoice")]
pub use native::{AsrStream, Recognizer, Vad};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sense_voice_json_full() {
        let raw = r#"{"lang": "<|zh|>", "emotion": "<|NEUTRAL|>", "event": "<|Speech|>", "text": "欢迎大家来体验达摩院推出的语音识别模型。", "timestamps": [0.96, 1.14], "durations": [], "tokens": ["欢", "迎"], "ys_log_probs": [], "words": []}"#;
        let seg = parse_sense_voice_json(raw).expect("parse");
        assert_eq!(
            seg.text,
            "欢迎大家来体验达摩院推出的语音识别模型。"
        );
        assert_eq!(seg.emotion.as_deref(), Some("NEUTRAL"));
        assert_eq!(seg.event.as_deref(), Some("Speech"));
        assert_eq!(seg.tokens, vec!["欢".to_string(), "迎".to_string()]);
        assert_eq!(seg.timestamps.len(), 2);
        assert!((seg.timestamps[0] - 0.96).abs() < 1e-6);
    }

    #[test]
    fn sense_voice_json_missing_optional_fields() {
        let seg =
            parse_sense_voice_json(r#"{"lang": "<|zh|>", "text": "你好"}"#)
                .expect("parse");
        assert_eq!(seg.text, "你好");
        assert!(seg.emotion.is_none());
        assert!(seg.event.is_none());
        assert!(seg.tokens.is_empty() && seg.timestamps.is_empty());
    }

    #[test]
    fn sense_voice_json_bare_token_passthrough() {
        // 旧 addon 已剥离标签的场景：cleanSenseVoiceToken 原样放行
        let seg = parse_sense_voice_json(
            r#"{"lang": "zh", "emotion": "HAPPY", "event": "BGM", "text": "好"}"#,
        )
        .expect("parse");
        assert_eq!(seg.emotion.as_deref(), Some("HAPPY"));
        assert_eq!(seg.event.as_deref(), Some("BGM"));
    }

    #[test]
    fn sense_voice_json_invalid_returns_none() {
        assert!(parse_sense_voice_json("not json").is_none());
        assert!(parse_sense_voice_json(r#"{"lang": "<|zh|>"}"#).is_none()); // 缺 text
    }

    #[test]
    fn model_path_join() {
        assert_eq!(
            sense_voice_model_path("/tmp/models"),
            "/tmp/models/model.int8.onnx"
        );
    }
}
