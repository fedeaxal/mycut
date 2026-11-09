//! 「编辑封面」页面:reaction 的封面编辑实例,嵌在本窗口 body 里。
//!
//! 打开自分P导出弹窗的「编辑封面…」。构建方式与 Reaction 页相同
//! (见 `panels::reaction`):同一上下文、同一 wgpu 设备、同一份设置与
//! 项目库——封面场景就存在 Reaction 的 project.db 里,下次打开还在。
//! 差别在 `new_embedded_cover`:固定布局(图片选择/来源/属性,无音频),
//! 且本实例存在期间音频设备不重复占用。
//!
//! 页面自己画一条顶栏(标题、画布信息、取消/保存封面),其余 body 交给
//! reaction 的 think + paint。保存 = 截下一合成帧(reaction 的截图分支写成
//! PNG),挪进 `<data dir>/cover-drafts/` 再把路径回给导出弹窗。

use std::path::PathBuf;

use egui::{Align2, Rect, pos2, vec2};

use crate::FilmcraftApp;
use crate::state::Mode;
use crate::theme::Tokens;

/// 顶栏高度(点)。
const BAR_HEIGHT: f32 = 40.0;
/// 截图等多久算超时(合成器没有出帧的极端情况)。
const SHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 封面编辑器的一个打开实例。
pub struct CoverEditor {
    obs: reaction::ObsApp,
    /// 打开时所处的模式——用户点了别的标签页就把编辑器收掉,不留一个
    /// 盖在别的页面上的全屏画布。
    opened_in: Mode,
    /// 已请求截图、在等报告;记录请求时刻用于超时。
    waiting_shot: Option<std::time::Instant>,
}

/// 打开封面编辑器(设置与目录的初始化和 Reaction 页一致)。
///
/// `recording_scene` 是这份视频出自的录制场景(分P弹窗从项目路径认出;
/// 不是录制项目就是 `None`)。编辑器按它取自动标题的名字、只列该场景
/// 的图片对——文字与场景的对应同 opencut 的 sceneName/emotionsDir。
pub fn open(app: &mut FilmcraftApp, ctx: &egui::Context, recording_scene: Option<String>) -> CoverEditor {
    if app.reaction_logging.is_none() {
        // 与 Reaction 页同一条日志路径:全部记录都进宿主的日志文件。
        app.reaction_logging = Some(reaction::log::EmbeddedLogging::start());
    }
    // 首次设置生效;Reaction 页设置过的话这里说的是同一批目录。
    if let Some(data_dir) = app.data_dir.as_ref() {
        let logs_dir = app.reaction_log_dir.clone().unwrap_or_else(|| data_dir.join("Logs"));
        reaction::paths::set_host_directories(data_dir.join("Reaction"), logs_dir);
    }
    let settings_store = reaction::SettingsStore::for_current_user();
    let settings = settings_store.load().unwrap_or_else(|error| {
        log::error!("could not load reaction settings: {error}");
        reaction::AppSettings::default()
    });
    // 与 Reaction 页同理:引擎设备与宿主合成器是否可用该设备无关。
    let render_state = app.wgpu_render_state.clone();
    CoverEditor {
        // 语言跟随宿主,映射与 Reaction 页共用(reaction::host_locale)。
        obs: reaction::ObsApp::new_embedded_cover(
            ctx,
            render_state,
            settings_store,
            settings,
            crate::panels::reaction::host_locale(app),
            recording_scene,
        ),
        opened_in: app.ui.mode,
        waiting_shot: None,
    }
}

/// 每帧画封面编辑器(打开时覆盖 body)。内部接管 `app.cover_editor` 的
/// 取放,模式切换或保存完成时把它放回 None。
pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, body: Rect) {
    let Some(mut cover) = app.cover_editor.take() else { return };
    // 用户点了别的页签:收掉编辑器,回到分P弹窗。
    if app.ui.mode != cover.opened_in {
        cover.obs.restore_cover_scene();
        app.ui.export.parts.open = true;
        app.cover_editor = None;
        return;
    }
    let t = app.tokens;
    let canvas = (reaction::domain::SceneCanvas::DEFAULT.width, reaction::domain::SceneCanvas::DEFAULT.height);

    // 顶栏:标题 + 画布信息 | 取消 / 保存封面。
    let bar = Rect::from_min_size(body.min, vec2(body.width(), BAR_HEIGHT));
    ui.painter().rect_filled(bar, 0.0, t.header_bg);
    ui.painter().text(
        pos2(bar.min.x + 14.0, bar.center().y),
        Align2::LEFT_CENTER,
        tl!("Cover Editor"),
        Tokens::semibold(13.0),
        t.text,
    );
    ui.painter().text(
        pos2(bar.min.x + 130.0, bar.center().y),
        Align2::LEFT_CENTER,
        tlf!("{w}×{h} · 16:9 · {guide}", w = canvas.0, h = canvas.1, guide = tl!("4:3 safe area")),
        Tokens::ui(11.5),
        t.text_dim,
    );
    let save_btn = Rect::from_min_max(egui::pos2(bar.max.x - 118.0, bar.min.y + 6.0), egui::pos2(bar.max.x - 8.0, bar.max.y - 6.0));
    let cancel_btn = Rect::from_min_max(egui::pos2(save_btn.min.x - 78.0, save_btn.min.y), egui::pos2(save_btn.min.x - 8.0, save_btn.max.y));
    let sresp = ui.interact(save_btn, egui::Id::unique("cover-save"), egui::Sense::click());
    ui.painter().rect_filled(save_btn, 12.0, if sresp.hovered() { t.accent_hover } else { t.accent });
    ui.painter().text(save_btn.center(), Align2::CENTER_CENTER, tl!("Save Cover"), Tokens::semibold(12.5), egui::Color32::WHITE);
    let cresp = ui.interact(cancel_btn, egui::Id::unique("cover-cancel"), egui::Sense::click());
    if cresp.hovered() {
        ui.painter().rect_stroke(cancel_btn, 12.0, egui::Stroke::new(1.0, t.text), egui::StrokeKind::Inside);
    }
    ui.painter().text(cancel_btn.center(), Align2::CENTER_CENTER, tl!("Cancel"), Tokens::ui(12.5), t.text);
    let save = sresp.clicked() && cover.waiting_shot.is_none();
    let cancel = cresp.clicked();

    // 截图报告轮询:成功 → 挪进草稿目录、回传弹窗;失败 → 状态栏报错,
    // 停下等待(可再点保存)。
    if let Some(asked) = cover.waiting_shot {
        match cover.obs.cover_shot() {
            Some(Ok(path)) => {
                let saved = park_cover_draft(app, &path);
                match saved {
                    Ok(draft) => {
                        app.ui.export.parts.cover_path = Some(draft.to_string_lossy().to_string());
                        app.ui.export.parts.set_cover_dirty();
                        cover.obs.restore_cover_scene();
                        app.ui.export.parts.open = true;
                        app.ui.status = tlf!("Cover saved: {path}", path = draft.display());
                        app.cover_editor = None;
                        return;
                    }
                    Err(e) => {
                        app.ui.status = e;
                        cover.waiting_shot = None;
                    }
                }
            }
            Some(Err(e)) => {
                app.ui.status = tlf!("Cover screenshot failed: {e}", e = e);
                cover.waiting_shot = None;
            }
            None if asked.elapsed() > SHOT_TIMEOUT => {
                app.ui.status = tl!("Cover screenshot timed out").into();
                cover.waiting_shot = None;
            }
            None => {}
        }
    }
    if save {
        cover.obs.request_cover_shot();
        cover.waiting_shot = Some(std::time::Instant::now());
    }
    if cancel {
        cover.obs.restore_cover_scene();
        app.ui.export.parts.open = true;
        app.cover_editor = None;
        return;
    }

    // body:reaction 的封面编辑界面。
    cover.obs.think(ui.ctx());
    let rest = Rect::from_min_max(egui::pos2(body.min.x, bar.max.y), body.max);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rest).id_salt("cover_body"));
    cover.obs.paint(&mut child);
    app.cover_editor = Some(cover);
}

/// 把截图从录制目录挪进 `<data dir>/cover-drafts/封面_<epoch 秒>.png`,
/// 返回草稿路径。同盘 rename,跨盘退化为复制。
fn park_cover_draft(app: &FilmcraftApp, shot: &std::path::Path) -> Result<PathBuf, String> {
    let dir = app.data_dir.as_deref().map(|d| d.join("cover-drafts")).unwrap_or_else(|| std::env::temp_dir().join("cover-drafts"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("cover-drafts: {e}"))?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let out = dir.join(format!("封面_{stamp}.png"));
    let moved = std::fs::rename(shot, &out).or_else(|_| std::fs::copy(shot, &out).map(|_| ())).map_err(|e| format!("cover draft: {e}"));
    moved.map(|_| out)
}
