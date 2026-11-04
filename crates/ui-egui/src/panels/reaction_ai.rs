//! The Reaction AI settings: the gear at the right end of the header while
//! Reaction is showing, and the dialog it opens, laid out like its opencut
//! counterpart — the OpenAI-format endpoint the page's AI features talk to,
//! the compliance screening's switches, and the speech-recognition model's
//! own section with its download. The values are this machine's (they live
//! in `preferences.json` beside the rest of the preferences, never in the
//! project), so the dialog says so.
//!
//! The talking to AI endpoints and the model download both run through the
//! `ai-gateway` crate — the same request assembly and the same ModelScope
//! downloader opencut uses, on threads of this process's own.

use std::sync::{Arc, Mutex};

use egui::{Align2, Color32};

use crate::FilmcraftApp;

/// How long a connection test may take before it is called unreachable —
/// named here because the timeout message under the form says the number.
const TEST_TIMEOUT_MS: f64 = 15_000.0;

/// The caption recognition languages the SenseVoice model distinguishes, in
/// the order the dropdown lists them — Chinese first, being the default, so
/// a stored value the list does not know falls back to the entry that is in
/// force. `auto` is the explicit never-force option, last. The ids are what
/// the recognizer takes (and what gets saved); the labels are translated at
/// the dropdown, and listed for the i18n catalog scan (see
/// `i18n::tests::dynamic_names`).
pub(crate) const SUBTITLE_LANGUAGES: &[(&str, &str)] = &[
    ("zh", "Chinese"),
    ("en", "English"),
    ("ja", "Japanese"),
    ("ko", "Korean"),
    ("yue", "Cantonese"),
    ("auto", "Auto detect"),
];

/// Draft of the dialog, on [`FilmcraftApp`] between the click that opens it
/// and the frames that draw it. `Default` is a closed dialog.
#[derive(Clone, Default)]
pub struct ReactionAiState {
    base_url: String,
    model: String,
    api_key: String,
    show_key: bool,
    compliance_enabled: bool,
    compliance_platforms: Vec<String>,
    /// The caption recognition language (see [`SUBTITLE_LANGUAGES`]).
    asr_language: String,
    /// A connection test in flight: the slot the worker thread writes and
    /// each frame here drains. `None` while no test runs.
    test: Option<Arc<Mutex<Option<Result<String, String>>>>>,
    /// The last test's answer, or why the form cannot be sent yet — under
    /// the form until the next attempt or the dialog closes.
    test_result: Option<Result<String, String>>,
    /// Where the ASR model files live (beside the rest of the Reaction
    /// data), and what the dialog knows about them.
    asr_dir: std::path::PathBuf,
    asr_ready: bool,
    /// A download in flight: the events its thread emits, drained each
    /// frame. `None` while none runs.
    asr_events: Option<Arc<Mutex<Vec<serde_json::Value>>>>,
    asr_downloading: bool,
    /// Aggregate progress of the two model files, and the one still going.
    asr_received: u64,
    asr_total: u64,
    asr_file: String,
    /// Why the last download failed, if it did.
    asr_error: Option<String>,
}

impl ReactionAiState {
    /// Opens the dialog: the form starts from what is saved, and the model
    /// section from what is on disk (a completeness check — never a
    /// download, which stays the user's own button).
    pub fn open(
        &mut self,
        saved: &filmcraft_engine::settings::ReactionAiPrefs,
        asr_dir: std::path::PathBuf,
    ) {
        *self = Self {
            base_url: saved.base_url.clone(),
            model: saved.model.clone(),
            api_key: saved.api_key.clone(),
            show_key: false,
            compliance_enabled: saved.compliance_enabled,
            compliance_platforms: saved.compliance_platforms.clone(),
            asr_language: saved.asr_language.clone(),
            test: None,
            test_result: None,
            asr_ready: ai_gateway::asr_model_status(&asr_dir),
            asr_dir,
            ..Self::default()
        };
    }

    /// The normalized form, or why it cannot be saved yet — the same check
    /// the test runs, so what a successful save proves is what was typed.
    fn config(&self) -> Result<filmcraft_engine::settings::ReactionAiPrefs, String> {
        let base_url = normalized_base_url(&self.base_url);
        let model = self.model.trim().to_string();
        if base_url.is_empty() {
            return Err(tl!("Please fill in the Base URL").to_string());
        }
        if model.is_empty() {
            return Err(tl!("Please fill in the model").to_string());
        }
        Ok(filmcraft_engine::settings::ReactionAiPrefs {
            base_url,
            model,
            api_key: self.api_key.trim().to_string(),
            compliance_enabled: self.compliance_enabled,
            compliance_platforms: self.compliance_platforms.clone(),
            asr_language: self.asr_language.trim().to_string(),
        })
    }

    /// Starts a connection test: one minimal chat completion through the
    /// gateway, on a thread of its own so the dialog stays responsive.
    fn start_test(draft: &mut ReactionAiState) {
        let Ok(config) = draft.config() else {
            return;
        };
        let payload = serde_json::json!({
            "config": {
                "baseUrl": config.base_url,
                "model": config.model,
                "apiKey": config.api_key,
            },
            "timeoutMs": TEST_TIMEOUT_MS,
        });
        let slot = Arc::new(Mutex::new(None));
        let emit_slot = Arc::clone(&slot);
        std::thread::Builder::new()
            .name("reaction-ai-test".into())
            .spawn(move || {
                let envelope = ai_gateway::invoke("ping", &payload.to_string());
                let outcome = serde_json::from_str::<serde_json::Value>(&envelope)
                    .ok()
                    .map(|envelope| {
                        if envelope["ok"].as_bool().unwrap_or(false) {
                            Ok(tlf!("Model {model} is reachable", model = config.model))
                        } else {
                            Err(envelope["error"].as_str().unwrap_or("error").to_string())
                        }
                    })
                    .unwrap_or_else(|| Err("ai-gateway 内部错误".to_string()));
                if let Ok(mut slot) = emit_slot.lock() {
                    *slot = Some(outcome);
                }
            })
            .ok();
        draft.test = Some(slot);
        draft.test_result = None;
    }

    /// Starts the model download through the gateway's ModelScope
    /// downloader: one thread, progress events into a slot this frame
    /// drains. The caller has made sure none is running.
    fn start_download(draft: &mut ReactionAiState) {
        let payload = serde_json::json!({ "dir": draft.asr_dir.display().to_string() });
        let slot = Arc::new(Mutex::new(Vec::new()));
        std::thread::Builder::new()
            .name("reaction-asr-download".into())
            .spawn({
                let slot = Arc::clone(&slot);
                move || {
                    ai_gateway::run_download("asr_model_ensure", &payload, Arc::new(move |event| {
                        if let Ok(mut events) = slot.lock() {
                            events.push(event);
                        }
                    }));
                }
            })
            .ok();
        draft.asr_events = Some(slot);
        draft.asr_downloading = true;
        draft.asr_error = None;
        draft.asr_received = 0;
        draft.asr_total = 0;
        draft.asr_file = String::new();
    }

    /// What the ASR section's status line says, in opencut's own terms:
    /// ready, downloading (with a percentage when the sizes are known), the
    /// failure, or nothing downloaded yet.
    fn asr_summary(&self) -> (String, Option<Color32>) {
        let mb = |bytes: u64| bytes / 1_048_576;
        if self.asr_downloading {
            if self.asr_total > 0 {
                let percent = (self.asr_received as f64 / self.asr_total as f64 * 100.0) as u64;
                (
                    tlf!(
                        "{file} {percent}% ({received} / {total} MB)",
                        file = self.asr_file.clone(),
                        percent,
                        received = mb(self.asr_received),
                        total = mb(self.asr_total),
                    ),
                    None,
                )
            } else {
                (
                    tlf!("Downloading {mb} MB", mb = mb(self.asr_received)),
                    None,
                )
            }
        } else if let Some(error) = &self.asr_error {
            (error.clone(), Some(Color32::from_rgb(0xE5, 0x48, 0x4D)))
        } else if self.asr_ready {
            (tl!("Ready").to_string(), None)
        } else {
            (tl!("Not downloaded").to_string(), None)
        }
    }
}

/// Trimmed, given a scheme when it lacks one, and stripped of its trailing
/// slashes — what gets pasted into an address field is usually one `/` away
/// from working, and the endpoint is joined as `{base}/chat/completions`.
fn normalized_base_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// What a field row's label column keeps, opencut's `w-20` (80 px).
const FIELD_LABEL_WIDTH: f32 = 80.0;
/// The dialog body's fixed width, and the switch cards' inner width — an
/// auto-sized window cannot be asked for its "available" width (that is the
/// whole screen before the first layout), so everything here is fixed.
const CONTENT_WIDTH: f32 = 430.0;
const CARD_WIDTH: f32 = 410.0;
/// 开关卡片文字行 / 按钮行的显式高度:RTL 布局的光标会扩张到容器剩余
/// 高度,自动尺寸弹窗里那就是全屏,必须显式框住。
const CARD_ROW_HEIGHT: f32 = 34.0;
const BUTTON_ROW_HEIGHT: f32 = 30.0;
/// The text fields' width: the content minus the label column and gap.
const FIELD_WIDTH: f32 = 330.0;

/// One labelled field row's label, in the shape opencut's `w-20 label +
/// input` rows take: fixed-width column, small text.
fn field_label(ui: &mut egui::Ui, text: impl AsRef<str>) {
    let text = text.as_ref();
    ui.add_sized(
        [FIELD_LABEL_WIDTH, 18.0],
        egui::Label::new(egui::RichText::new(text).text_style(egui::TextStyle::Small)),
    );
}

/// A switch row as opencut draws it: a bordered card with the title and its
/// description on the left, the toggle on the right.
fn switch_card(
    ui: &mut egui::Ui,
    checked: &mut bool,
    title: impl AsRef<str>,
    description: impl AsRef<str>,
    width: f32,
) {
    let (title, description) = (title.as_ref(), description.as_ref());
    egui::Frame::default()
        .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(width);
            // Horizontal with an explicit height and an explicit width:
            // the toggle is pushed to the card's right edge by burning the
            // leftover width as space. No right-to-left layout here — its
            // sizing pass and paint pass disagree about where the toggle
            // sits in an auto-sized dialog, and a toggle you cannot click
            // is worse than a left-aligned one.
            ui.horizontal(|ui| {
                ui.set_height(CARD_ROW_HEIGHT);
                ui.vertical(|ui| {
                    ui.set_max_height(CARD_ROW_HEIGHT);
                    ui.label(egui::RichText::new(title).text_style(egui::TextStyle::Small));
                    ui.label(
                        egui::RichText::new(description)
                            .text_style(egui::TextStyle::Small)
                            .color(ui.visuals().weak_text_color()),
                    );
                });
                // 开关固定在卡片右缘的确定矩形里(绘制与命中同区,量测趟
                // 与绘制趟也一致)。
                let row_top = ui.cursor().top();
                let rect = egui::Rect::from_center_size(
                    egui::pos2(ui.max_rect().right() - 12.0, row_top + CARD_ROW_HEIGHT / 2.0),
                    egui::vec2(18.0, 18.0),
                );
                ui.put(rect, egui::Checkbox::without_text(checked));
            });
        });
}

/// Draws the dialog. `false` once it has closed itself — Escape, the X,
/// Cancel or OK — which is the caller's cue to drop it from `app.dialog`.
pub fn show(app: &mut FilmcraftApp, ctx: &egui::Context) -> bool {
    // Taken out rather than borrowed: the drawing wants all of `app` (the
    // preferences to save into, the tokens to paint with) and the draft at
    // once, which one borrow of the struct cannot give both.
    let mut state = std::mem::take(&mut app.reaction_ai);
    let keep = state.draw(app, ctx);
    app.reaction_ai = state;
    keep
}

impl ReactionAiState {
    fn draw(&mut self, app: &mut FilmcraftApp, ctx: &egui::Context) -> bool {
        // A test that finished since the last frame stops being in flight and
        // its answer shows under the form.
        if let Some(slot) = &self.test
            && let Some(outcome) = slot.lock().ok().and_then(|mut slot| slot.take())
        {
            self.test = None;
            self.test_result = Some(outcome);
        }
        // The download's events, in the order they were emitted: status
        // updates move the progress line, the result event ends it.
        if let Some(slot) = &self.asr_events
            && let Ok(mut events) = slot.lock()
        {
            for event in events.drain(..) {
                match event["event"].as_str() {
                    Some("status") => match event["state"].as_str() {
                        Some("ready") => {
                            self.asr_downloading = false;
                            self.asr_ready = true;
                            self.asr_received = 0;
                            self.asr_total = 0;
                        }
                        Some("downloading") => {
                            self.asr_downloading = true;
                            self.asr_received = event["received"].as_u64().unwrap_or(0);
                            self.asr_total = event["total"].as_u64().unwrap_or(0);
                            self.asr_file = event["file"].as_str().unwrap_or("").to_string();
                        }
                        _ => {}
                    },
                    Some("result") => {
                        self.asr_downloading = false;
                        if event["ok"].as_bool().unwrap_or(false) {
                            self.asr_ready = true;
                        } else {
                            self.asr_error = Some(
                                event["error"].as_str().unwrap_or("error").to_string(),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }

        let t = app.tokens;
        let mut open = true;
        let mut keep = true;
        let mut apply = false;
        let testing = self.test.is_some();
        egui::Window::new(tl!("AI Settings"))
            .id(egui::Id::unique("Reaction AI Settings"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(Align2::CENTER_CENTER)
            .default_pos(crate::widgets::dialog_center(ctx))
            .show(ctx, |ui| {
                // 弹窗内容闭包每帧跑两趟(量测趟 + 绘制趟):量测趟画在可
                // 丢弃的克隆上,&mut 控件被切换两次等于没切。只有真趟才落
                // 状态、起线程。
                let real = !ui.is_sizing_pass();
                let mut scratch = if real { None } else { Some(self.clone()) };
                let draft: &mut ReactionAiState = if real {
                    self
                } else {
                    scratch.as_mut().expect("sizing pass owns a clone")
                };
                ui.add_space(4.0);
                ui.label(tl!(
                    "Configure the OpenAI-format interface the Reaction AI uses. Saved on this machine only."
                ));
                ui.set_width(CONTENT_WIDTH);
                ui.add_space(8.0);
                egui::Grid::new("reaction-ai-fields")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        field_label(ui, tl!("Base URL"));
                        ui.add(
                            egui::TextEdit::singleline(&mut draft.base_url)
                                .hint_text("https://api.openai.com/v1")
                                .desired_width(FIELD_WIDTH),
                        );
                        ui.end_row();
                        field_label(ui, tl!("Model"));
                        ui.add(
                            egui::TextEdit::singleline(&mut draft.model)
                                .hint_text("gpt-4o-mini")
                                .desired_width(FIELD_WIDTH),
                        );
                        ui.end_row();
                        field_label(ui, tl!("API Key"));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut draft.api_key)
                                    .hint_text(tl!("sk-… (leave empty for a local service)"))
                                    .password(!draft.show_key)
                                    .desired_width(240.0),
                            );
                            if ui
                                .small_button(if draft.show_key { tl!("Hide") } else { tl!("Show") })
                                .clicked()
                            {
                                draft.show_key = !draft.show_key;
                            }
                        });
                        ui.end_row();
                    });

                // 录制中违规检测 — a section of its own under a rule, the
                // switch immediately effective for the next recording, the
                // platform chips keeping at least one entry (an empty set
                // would read as a broken switch rather than a choice).
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(tl!(
                        "Compliance checks while recording (wordlists against the recognized text; AI vision and semantic screening follow the scene's Use AI switch)"
                    ))
                    .text_style(egui::TextStyle::Small)
                    .color(t.text_dim),
                );
                ui.add_space(4.0);
                switch_card(
                    ui,
                    &mut draft.compliance_enabled,
                    tl!("Enable compliance checks (local wordlists, alerts as you record)"),
                    tl!("Local wordlists cost nothing; a hit alerts as you record"),
                    CARD_WIDTH,
                );
                ui.horizontal(|ui| {
                    field_label(ui, tl!("Target platforms"));
                    for (id, label) in filmcraft_engine::settings::COMPLIANCE_PLATFORMS {
                        let active = draft.compliance_platforms.iter().any(|p| p == id);
                        let toggled = ui
                            .add_enabled(
                                draft.compliance_enabled,
                                egui::Button::new(crate::i18n::t(label)).selected(active),
                            )
                            .clicked();
                        if toggled && (active || draft.compliance_platforms.len() > 1) {
                            if active {
                                draft.compliance_platforms.retain(|p| p != id);
                            } else {
                                draft.compliance_platforms.push((*id).to_string());
                            }
                        }
                    }
                });

                // 语音识别模型 — the gateway's ModelScope downloader behind
                // the user's own button, progress line where opencut shows
                // the same aggregate.
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(tl!(
                        "Speech recognition model (for live captions, ~240MB, downloaded from ModelScope)"
                    ))
                    .text_style(egui::TextStyle::Small)
                    .color(t.text_dim),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    field_label(ui, tl!("Status"));
                    let (summary, colour) = draft.asr_summary();
                    let mut line = egui::RichText::new(summary).text_style(egui::TextStyle::Small);
                    if let Some(colour) = colour {
                        line = line.color(colour);
                    }
                    ui.label(line);
                    let label = if draft.asr_downloading {
                        tl!("Downloading…").to_string()
                    } else if draft.asr_ready {
                        tl!("Downloaded").to_string()
                    } else if draft.asr_error.is_some() {
                        tl!("Retry download").to_string()
                    } else {
                        tl!("Download model").to_string()
                    };
                    if ui
                        .add_enabled(!draft.asr_downloading, egui::Button::new(label).small())
                        .clicked()
                        && !draft.asr_downloading
                        && !draft.asr_ready
                    {
                        ReactionAiState::start_download(draft);
                    }
                });
                if draft.asr_downloading && draft.asr_total > 0 {
                    let fraction =
                        (draft.asr_received as f32 / draft.asr_total.max(1) as f32).clamp(0.0, 1.0);
                    ui.add(
                        egui::ProgressBar::new(fraction)
                            .desired_width(CARD_WIDTH)
                            .show_percentage(),
                    );
                }

                // 字幕识别语言 — the model distinguishes five languages; the
                // choice is the captions' only, which the note below says so
                // nobody expects the AI's own recognition to follow it.
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    field_label(ui, tl!("Subtitle language"));
                    let selected = SUBTITLE_LANGUAGES
                        .iter()
                        .find(|(code, _)| *code == draft.asr_language.trim())
                        .unwrap_or(&SUBTITLE_LANGUAGES[0]);
                    egui::ComboBox::from_id_salt("reaction_ai_subtitle_language")
                        .width(FIELD_WIDTH - 12.0)
                        .selected_text(crate::i18n::t(selected.1))
                        .show_ui(ui, |ui| {
                            for (code, label) in SUBTITLE_LANGUAGES {
                                ui.selectable_value(
                                    &mut draft.asr_language,
                                    (*code).to_string(),
                                    crate::i18n::t(label),
                                );
                            }
                        });
                });
                ui.label(
                    egui::RichText::new(tl!("Captions are recognized in this language; the audio that is sent to the AI keeps auto-detect."))
                    .text_style(egui::TextStyle::Small)
                    .color(t.text_dim),
                );

                if let Some(result) = &draft.test_result {
                    ui.add_space(6.0);
                    let (text, colour) = match result {
                        Ok(text) => (text.clone(), t.accent),
                        Err(text) => (text.clone(), Color32::from_rgb(0xE5, 0x48, 0x4D)),
                    };
                    ui.label(egui::RichText::new(text).color(colour));
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.set_height(BUTTON_ROW_HEIGHT);
                    let test = ui.add_enabled(
                        !testing,
                        egui::Button::new(if testing {
                            tl!("Testing…").to_string()
                        } else {
                            tl!("Test Connection").to_string()
                        }),
                    );
                    if test.clicked() && !testing {
                        match draft.config() {
                            Ok(_) => ReactionAiState::start_test(draft),
                            Err(e) => draft.test_result = Some(Err(e)),
                        }
                    }
                    if ui.button(tl!("Cancel")).clicked() {
                        keep = false;
                    }
                    let ok = ui.add(
                        egui::Button::new(egui::RichText::new(tl!("OK")).color(Color32::WHITE))
                            .fill(app.tokens.accent)
                            .min_size(egui::vec2(64.0, 20.0)),
                    );
                    if ok.clicked() {
                        apply = true;
                    }
                    // 把两个按钮推到右缘:剩余宽度全部交给间隔(LTR,两趟一致)。
                    let rest = ui.available_width().max(0.0);
                    ui.add_space(rest);
                });
            });
        if !open || crate::widgets::escape_closes(ctx) {
            keep = false;
        }
        if apply {
            match self.config() {
                // Saving is only refused where the form is incomplete, and
                // then the dialog stays up with the reason under the form —
                // the same answer the test would have given.
                Ok(config) => {
                    let mut prefs = app.session.prefs.clone();
                    prefs.reaction_ai = config.clone();
                    if let Err(e) = app.session.set_prefs(prefs) {
                        app.ui.status = e.to_string();
                    }
                    // 同一份端点镜像给内嵌的 reaction(热词查询等用它),
                    // 它自己落自己的 settings 文件——两处内容一致。
                    if let Some(reaction) = app.reaction.as_mut() {
                        reaction.set_ai_endpoint(reaction::settings::AiEndpointConfig {
                            base_url: config.base_url,
                            model: config.model,
                            api_key: config.api_key,
                            asr_language: config.asr_language,
                        });
                    }
                    keep = false;
                }
                Err(e) => self.test_result = Some(Err(e)),
            }
        }
        keep
    }
}


