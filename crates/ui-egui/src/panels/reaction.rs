//! Reaction mode: reaction, running embedded in this window.
//!
//! The two halves of Studio are one process and one window now. This page
//! builds an `reaction::ObsApp` the first time Reaction is entered — on this
//! context, on this window's wgpu device — and draws its frames hereafter;
//! the wall's Reaction button is a mode switch rather than a program
//! launch. Leaving the page keeps the app alive (its engine and its
//! captures keep running, the way an reaction window keeps running when its
//! Preview is hidden); the header's tabs are the way back.

use crate::FilmcraftApp;

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, body: egui::Rect) {
    // 本页显示的语言:跟随宿主偏好。构建时喂给新实例,此后每帧喂给活着
    // 的实例(宿主在本进程里随时可换语言,换页回来即跟上),见
    // `ObsApp::set_locale`。
    let locale = host_locale(app);
    // Built on first entry: the engine's pipelines, captures and audio all
    // come up then, and nothing here tears them down on a tab switch.
    if app.reaction.is_none() {
        // Its records land in the host's log: this application's `tracing`
        // events are bridged onto the host's `log` logger, and media's
        // records are forwarded onto the same facade — one process, one
        // log, the host's settings governing both. Once per process, before
        // the app is built, so its start-up is written down.
        if app.reaction_logging.is_none() {
            app.reaction_logging = Some(reaction::log::EmbeddedLogging::start());
        }
        // Before anything of reaction reads or writes a file of its own: its
        // settings, project database and browser profile live under this
        // application's data directory — one program, one set of directories
        // — and its log directory is this application's own, the one the
        // host's log file lives in. First-set wins: later calls change
        // nothing.
        // `main` calls this first, before the browser engine starts, so this
        // names the same places again — and is only what takes where `main`
        // had no data directory to name.
        if let Some(data_dir) = app.data_dir.as_ref() {
            let logs_dir = app
                .reaction_log_dir
                .clone()
                .unwrap_or_else(|| data_dir.join("Logs"));
            reaction::paths::set_host_directories(data_dir.join("Reaction"), logs_dir);
        }
        // 内嵌 reaction 的 macOS 应用菜单随之说本应用的名字("关于 FilmCraft/
        // 退出 FilmCraft"而不是"关于 reaction"),About/Quit 经
        // `take_pending_app_menu_commands` 交还本页执行(见 show 末尾)。
        reaction::host::set_app_name("FilmCraft");
        let settings_store = reaction::SettingsStore::for_current_user();
        let mut settings = settings_store.load().unwrap_or_else(|error| {
            log::error!("could not load reaction settings: {error}");
            reaction::AppSettings::default()
        });
        // 端点真相源在宿主偏好:构建时即镜像进内嵌实例(此前只在 AI 设置
        // 对话框保存时镜像,用户只要本会话没重新保存过对话框,开始录制就
        // 会误报"AI 接口未配置")。只在宿主配置过 AI 的情况下覆盖——
        // reaction 自己的存量端点不被空偏好抹掉。
        let host_ai = &app.session.prefs.reaction_ai;
        if !host_ai.base_url.trim().is_empty() && !host_ai.model.trim().is_empty() {
            let mirrored = reaction::settings::AiEndpointConfig {
                base_url: host_ai.base_url.clone(),
                model: host_ai.model.clone(),
                api_key: host_ai.api_key.clone(),
                asr_language: host_ai.asr_language.clone(),
            };
            if settings.ai_endpoint != mirrored {
                settings.ai_endpoint = mirrored.clone();
                if let Err(error) = settings_store.save(&settings) {
                    log::error!("could not mirror the host AI endpoint: {error}");
                }
            }
        } else if settings.ai_endpoint.ready() {
            // 反向收编:宿主偏好从未配置过 AI 而 reaction 自己的存量端点
            // 可用(旧版本落盘的),把它搬进宿主偏好并持久化——AI 设置
            // 对话框读宿主偏好,不搬它显示的就是一片空,与录制实际在用的
            // 端点矛盾。收编一次后真相源回到宿主,两处从此一致。
            let adopted = filmcraft_engine::settings::ReactionAiPrefs {
                base_url: settings.ai_endpoint.base_url.clone(),
                model: settings.ai_endpoint.model.clone(),
                api_key: settings.ai_endpoint.api_key.clone(),
                asr_language: settings.ai_endpoint.asr_language.clone(),
                compliance_enabled: app.session.prefs.reaction_ai.compliance_enabled,
                compliance_platforms: app.session.prefs.reaction_ai.compliance_platforms.clone(),
            };
            let mut prefs = app.session.prefs.clone();
            prefs.reaction_ai = adopted;
            if let Err(error) = app.session.set_prefs(prefs) {
                log::error!("could not adopt the reaction AI endpoint into the host preferences: {error}");
            }
        }
        // 界面语言不再写入设置:主题与语言都不是这里的设置了,语言由
        // 宿主随构造传入并每帧跟随。
        // 界面扩展字体（opencut 的启动字体下载）：后台经 ai-gateway 的
        // jsdelivr 镜像链补齐到 <Reaction>/fonts，一次进程只跑一次；
        // 失败只留日志——字体是增强，缺了回退系统字体。
        let fonts_dir = reaction::paths::data_dir().join("fonts");
        std::thread::Builder::new()
            .name("reaction-fonts".into())
            .spawn(move || {
                // 系统字体扫描(属性面板下拉的数据源)读并解析全部字体
                // 文件的元数据,放在这条后台线上做,UI 线程首帧即取现成
                // 结果(filmcraft_text 内部有去重的 OnceLock)。
                filmcraft_text::fonts::scan_system();
                ai_gateway::run_download(
                    "web_fonts_ensure",
                    &serde_json::json!({ "dir": fonts_dir.display().to_string() }),
                    std::sync::Arc::new(|event| {
                        // 只有终态记日志:逐字体的失败由网关的连续失败早停
                        // 收敛,不然网络不可达时会刷一整屏。
                        match event["event"].as_str() {
                            Some("result") if !event["ok"].as_bool().unwrap_or(false) => {
                                log::warn!(
                                    "字体下载中止: {}",
                                    event["error"].as_str().unwrap_or("")
                                );
                            }
                            Some("status") if event["state"] == "aborted" => {
                                log::warn!(
                                    "字体下载提前终止: {}",
                                    event["error"].as_str().unwrap_or("")
                                );
                            }
                            _ => {}
                        }
                    }),
                );
            })
            .ok();
        // The window's own render state, not `gpu`'s: on a software rasterizer the GPU
        // compositor is dropped and `gpu` is None, while the state still works — the
        // engine builds its compositing device on this adapter whatever it is.
        let render_state = app.wgpu_render_state.clone();
        app.reaction = Some(reaction::ObsApp::new_embedded(
            ui.ctx(),
            render_state,
            settings_store,
            settings,
            // The global hotkeys need a window handle, which only the
            // standalone shell has; inside this window the page's own
            // shortcuts do the work.
            None,
            locale,
        ));
    }
    let Some(reaction) = app.reaction.as_mut() else { return };
    reaction.set_locale(locale);
    reaction.think(ui.ctx());
    // 停止后的项目交付:收尾线程把「视频+字幕+违规标记」的项目生成好
    // 后落在这里,宿主打开它并切到编辑界面——录制→剪辑的换手(opencut
    // 的 jumpToEditor 同一时刻)。
    if let Some(project_path) = reaction.take_pending_project_import() {
        match app.session.execute("file.open", serde_json::json!({ "path": project_path.display().to_string() })) {
            Ok(_) => {
                app.ui.mode = crate::state::Mode::Edit;
                // 新一段录制的新模板:清上一场的编辑标记,迟到的 AI 元数据
                // (标题/简介/标签,收尾线程仍在生成)才能回填投稿模板。
                app.ui.export.parts.reset_bili_edits();
            }
            Err(e) => {
                app.ui.status = format!("could not open the recording project: {e}");
            }
        }
    }
    // The body every other mode is handed: below the header, whose height
    // this page's top edge therefore matches, and on macOS the header is
    // also what carries the window's traffic lights in its left padding.
    // The embedded application draws the same picture here that its
    // standalone window would, fitted to what it is given.
    let mut child = ui
        .new_child(egui::UiBuilder::new().max_rect(body).id_salt("reaction_body"));
    reaction.paint(&mut child);
    // 应用菜单的"关于/退出":内嵌 reaction 的 macOS 应用菜单说本应用的名字
    // (reaction::host::set_app_name),用户点选后把命令交还到这里——走
    // activate_native,与原生菜单栏点选本应用自己的条目同一条路,所以
    // 出现的是本应用原本的关于对话框与退出流程。
    if let Some(reaction) = app.reaction.as_mut() {
        for command in reaction.take_pending_app_menu_commands() {
            let id = match command {
                reaction::HostAppMenuCommand::About => "app.about",
                reaction::HostAppMenuCommand::Quit => "app.quit",
            };
            if let Err(e) = crate::menus::activate_native(app, ui.ctx(), id) {
                app.ui.status = e;
            }
        }
    }
}

/// 宿主偏好里的界面语言 → Reaction 的语言包。filmcraft 支持而 Reaction
/// 也有语言包的按名称映射,其余(跟随系统、以及 Reaction 尚无语言包的
/// 语言)一律中文——中文是这个页面的缺省。封面编辑器(cover_editor)与
/// 本页共用这一个映射。
pub(crate) fn host_locale(app: &FilmcraftApp) -> reaction::i18n::Locale {
    match app.session.prefs.general.interface_language.as_str() {
        "en" => reaction::i18n::Locale::EnUs,
        "ko" => reaction::i18n::Locale::KoKr,
        _ => reaction::i18n::Locale::ZhCn,
    }
}

/// 录后补丁的就地应用:项目生成不等识别排水,宿主取到项目即跳编辑界
/// 面;排水落库的尾段字幕/发现随后装进补丁槽。补丁所属的项目仍是当前
/// 打开的,就把增量补进活动会话(captions/markers,时间线即时出现,不
/// 需要重开项目)并存盘——存的是会话当前状态,用户这几秒里的编辑一并
/// 保留。项目已被换走(用户在收尾这几秒里打开了别的项目)就丢弃补丁,
/// 不污染别家。
pub(crate) fn apply_project_patch(
    app: &mut FilmcraftApp,
    patch: reaction::project_import::ProjectTailPatch,
) {
    let open = app.session.path.as_deref().map(str::to_owned);
    if open.as_deref() != Some(patch.project_path.to_string_lossy().as_ref()) {
        log::warn!(
            "recording tail patch dropped: {} is not the open project",
            patch.project_path.display()
        );
        return;
    }
    let outcome = {
        let session = &mut app.session;
        patch.apply(&mut |id, params| session.execute(id, params).map_err(|e| e.to_string()))
    };
    if let Err(error) = outcome {
        app.ui.status = format!("could not add the recording tail: {error}");
    }
    if let Err(e) = app.session.execute("file.save", serde_json::json!({})) {
        app.ui.status = format!("could not save the recording tail: {e}");
    }
}
