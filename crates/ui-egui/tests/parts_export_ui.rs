//! Headless UI tests of the「匹配源 – 分P导出」dialog (Quick Export's extra
//! entry): opening it from the Quick Export popup, and starting an export of
//! the whole video plus parts into the queue — the dialog closes the moment
//! the export starts, and the in-flight settle writes the cover and the biliup
//! template next to the videos once everything is settled.

use std::path::{Path, PathBuf};use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    dir: PathBuf,
}

impl Driver {
    fn new(name: &str) -> Self {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/export-tests").join(format!("ui-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s.export_presets.set_dir(&dir.join("data"));
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(s).with_control(rx);
        let harness = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, dir };
        d.frames(4);
        d
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let v = self.call(method, params.clone());
        assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
        v["result"].clone()
    }

    fn exec(&mut self, command: &str, params: Value) -> Value {
        self.ok("engine.execute", json!({"command": command, "params": params}))
    }

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }

    fn has(&mut self, id: &str) -> bool {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        v.as_array().unwrap().iter().any(|e| e["id"] == json!(id))
    }

    /// [x, y, w, h] of a registered element.
    fn rect(&mut self, id: &str) -> [f32; 4] {
        let v = self.ok("ui.elements", json!({"prefix": id}));
        let e = v.as_array().unwrap().iter().find(|e| e["id"] == json!(id)).expect("element");
        let r = e["rect"].as_array().unwrap();
        [r[0].as_f64().unwrap() as f32, r[1].as_f64().unwrap() as f32, r[2].as_f64().unwrap() as f32, r[3].as_f64().unwrap() as f32]
    }

    /// 把 `id` 滚进弹窗的可视区:在滚动区里滚轮向下(`within` 是钉在
    /// 底部、永远不会被滚走的那个元素——落点取它上方 100pt,正是滚动
    /// 视口)。元素自己的矩形不能当落点:它可能正被裁在视口外。
    fn scroll_to(&mut self, id: &str, within: &str) {
        for _ in 0..12 {
            let w = self.rect(within);
            let target = self.rect(id);
            if target[1] + target[3] < w[1] - 8.0 && target[1] > 0.0 {
                return;
            }
            let at = [target[0] + target[2] / 2.0, w[1] - 100.0];
            // egui 的滚轮:正值向上滚(内容往下走),往下翻用负值。
            self.ok("ui.scroll", json!({"x": at[0], "y": at[1], "dy": -140.0}));
            self.frames(2);
        }
    }

    fn app(&mut self) -> &mut FilmcraftApp {
        self.harness.state_mut()
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Quick Export 的列表里有「匹配源 – 分P导出」,点了打开弹窗;弹窗里格
/// 式/画质/整段/分P/封面/投稿模板/输出文件夹俱在,取消关掉。
#[test]
fn the_parts_entry_in_quick_export_opens_the_dialog() {
    let mut d = Driver::new("parts-open");
    d.ok("ui.set", json!({"mode": "edit"}));
    d.frames(2);
    d.click("header.quickExport");
    assert!(d.has("quickExport.parts"), "the parts entry is in the popup");
    d.click("quickExport.parts");
    assert!(d.has("partsExport.parts"), "the dialog opened");
    for id in [
        "partsExport.format",
        "partsExport.quality",
        "partsExport.audio",
        "partsExport.whole",
        "partsExport.parts",
        "partsExport.prefix",
        "partsExport.separator",
        "partsExport.splitMode",
        "partsExport.splitMode.duration",
        "partsExport.splitMode.count",
        "partsExport.perPart",
        "partsExport.cover.pick",
        "partsExport.cover.edit",
        "partsExport.title",
        "partsExport.tags",
        "partsExport.desc",
        "partsExport.location",
        "partsExport.go",
    ] {
        assert!(d.has(id), "{id}");
    }
    assert!(d.app().ui.export.parts.open);
    // 每P时长默认 2 分钟;画质缺省「与源视频相同码率」。
    assert_eq!(d.app().ui.export.parts.per_part_minutes, 2.0);
    assert_eq!(d.app().ui.export.parts.quality, "source");
    // quick popup closed with the dialog opening
    assert!(!d.app().ui.export.quick_open);

    // opencut 的两个拆分方式始终并排可见;点「分P数量」切模式,数值行随之
    // 从「每P时长」换成「分P数量」,再点回「每P时长」。
    assert_eq!(d.app().ui.export.parts.split_mode, "duration");
    d.click("partsExport.splitMode.count");
    assert_eq!(d.app().ui.export.parts.split_mode, "count");
    assert!(d.has("partsExport.partCount"), "count mode shows its own row");
    d.click("partsExport.splitMode.duration");
    assert_eq!(d.app().ui.export.parts.split_mode, "duration");
    assert!(d.has("partsExport.perPart"), "duration mode shows its own row");

    d.click("partsExport.cancel");
    assert!(!d.app().ui.export.parts.open);
}

/// 标签输入照 opencut 的 TagInput:逗号(中英)即时分段、去重、上限 10、
/// 空草稿退格删最后一个。输入用 `ui.type` 打进焦点框。
#[test]
fn the_tag_input_splits_on_commas_like_opencut() {
    use filmcraft_ui_egui::panels::parts_export::PartsExportUi;

    let mut d = Driver::new("parts-tags");
    d.ok("ui.set", json!({"mode": "edit"}));
    d.frames(2);
    d.click("header.quickExport");
    d.click("quickExport.parts");
    // 标签行在弹窗滚动区的下半段:先滚进来(真实用户也是这样),再点。
    d.scroll_to("partsExport.tags", "partsExport.go");
    d.click("partsExport.tags");

    // 逗号分段:一次键入把 `reaction,测试，` 打成两个标签,草稿清空。
    for t in ["reaction", ",", "测试", "，"] {
        d.ok("ui.type", json!({"text": t}));
    }
    d.frames(2);
    let p: &PartsExportUi = &d.app().ui.export.parts;
    assert_eq!(p.bili_tags, vec!["reaction".to_string(), "测试".to_string()], "逗号即时分段");
    assert!(p.tag_draft.is_empty(), "分段后草稿清空: {:?}", p.tag_draft);

    // 去重(精确比较,与 opencut 的 includes 一致)与两端空白去掉。
    d.app().ui.export.parts.tag_draft = "  reaction  ".into();
    d.ok("ui.key", json!({"key": "Enter"}));
    d.frames(2);
    assert_eq!(d.app().ui.export.parts.bili_tags.len(), 2, "重复标签被跳过");

    // 上限 10:塞满后再提交一个不再增长。
    for i in 0..20 {
        d.app().ui.export.parts.tag_draft = format!("t{i}");
        d.ok("ui.key", json!({"key": "Enter"}));
        d.frames(1);
    }
    assert_eq!(d.app().ui.export.parts.bili_tags.len(), filmcraft_ui_egui::panels::parts_export::MAX_TAGS, "标签上限");

    // 空草稿退格删最后一个。
    d.click("partsExport.tags");
    let before = d.app().ui.export.parts.bili_tags.len();
    d.ok("ui.key", json!({"key": "Backspace"}));
    d.frames(2);
    assert_eq!(d.app().ui.export.parts.bili_tags.len(), before - 1, "空草稿退格删最后一个标签");
}

/// 开始导出:整段与分P同选时整段只渲染一次——队列里只有整段,分P变成
/// 对整段产物的切分清单(等整段完成后由 `export.split` 流复制切出);
/// 点「开始导出」弹窗立即关闭,收尾转到后台(`in_flight`)。随后取消——
/// demo 序列全长编码要几分钟,产物落盘由单元测试覆盖(`write_outputs`),
/// 这里验证的是入队、弹窗关闭与全部取消后的收尾(不写产物)。
#[test]
fn starting_an_export_queues_whole_and_parts_and_cancels() {
    let mut d = Driver::new("parts-go");
    d.ok("ui.set", json!({"mode": "edit"}));
    d.frames(2);
    d.click("header.quickExport");
    d.click("quickExport.parts");

    let dir = d.dir.join("parts-out");
    std::fs::create_dir_all(&dir).unwrap();
    {
        // 每P取序列时长的 ~2/5:无论 demo 序列多长都拆出 2-3 段。
        let duration = d.app().session.active_sequence().map(|q| q.duration().seconds()).unwrap_or(60.0).max(10.0);
        let p = &mut d.app().ui.export.parts;
        p.location = dir.to_string_lossy().to_string();
        p.per_part_minutes = (duration / 60.0 * 0.4).max(1.0 / 60.0);
        p.cover_path = None;
        p.bili_title = "测试投稿".into();
        p.bili_tags = vec!["reaction".into(), "测试".into()];
    }
    d.frames(2);

    let before = d.exec("export.queue.list", json!({}))["items"].as_array().unwrap().len();
    d.click("partsExport.go");
    // 开始导出:弹窗立即关闭,收尾交给后台轮询。
    assert!(!d.app().ui.export.parts.open, "开始导出后弹窗应立即关闭");
    assert!(d.app().ui.export.parts.in_flight);
    d.frames(4);

    // 合并路:队列只新增整段一个渲染输出;分P是切分计划,不在队列里。
    let items = d.exec("export.queue.list", json!({}))["items"].as_array().unwrap().clone();
    // 拷出来再往下借:后面还要经 d.app() 轮询进度,不能一直借住在它身上。
    let queued: Vec<(String, String)> = d.app().ui.export.parts.items.iter().map(|i| (i.file.clone(), i.path.clone())).collect();
    let planned: Vec<String> = d.app().ui.export.parts.planned_parts.iter().map(|p| p.item.file.clone()).collect();
    assert_eq!(items.len(), before + queued.len());
    assert_eq!(queued.len(), 1, "整段只渲染一次(字幕/效果/画质都在里面)");
    assert!(!planned.is_empty(), "分P变成对整段产物的切分计划");
    assert!(planned.iter().any(|f| f.contains("P1")), "切分计划带 P 号文件名: {planned:?}");
    assert!(queued[0].0.ends_with(".mp4") && !queued[0].0.ends_with(".mp4.mp4"), "整段文件名不带双扩展名: {}", queued[0].0);

    // 取消全部(进度与重试在导出模式的导出队列里,这里直接走引擎命令):
    // 等后台收尾放行 in_flight;全部取消不写产物,也没有可切分的产物。
    d.exec("export.queue.cancel", json!({}));
    let t0 = std::time::Instant::now();
    while d.app().ui.export.parts.in_flight {
        assert!(t0.elapsed().as_secs() < 120, "timed out waiting for the cancellation to settle");
        d.frames(4);
    }
    let base = d.app().ui.export.parts.base_name.clone();
    assert!(!base.is_empty(), "the base name was set when the export started");
    assert!(!dir.join(format!("{base}.biliup.json")).is_file(), "nothing was written after cancelling everything");
    assert!(!d.app().ui.export.parts.open);
}

/// 投稿模板旁车随项目走:项目目录里的 `<场景名>.publish.json`(reaction
/// 录后 AI 落的)在重开项目后灌回弹窗;用户手改后整份回写旁车——
/// 重开、重启都不丢。
#[test]
fn the_upload_template_sidecar_reopens_with_the_project_and_records_user_edits() {
    // 第一个 app:把项目存到弹窗目录,再模拟 reaction 落的旁车。
    let mut d = Driver::new("parts-sidecar");
    let doc = d.dir.join("p.fcproj").to_string_lossy().into_owned();
    d.exec("file.saveAs", json!({"path": doc}));
    let sidecar = d.dir.join("日常.publish.json");
    std::fs::write(
        &sidecar,
        json!({"title": "上一场的标题", "description": "上一场的简介", "tags": ["reaction"]}).to_string(),
    )
    .unwrap();

    // 重开:全新 app 打开同一个项目文件,打开弹窗即预填。
    let mut s = Session::default();
    s.execute("file.open", json!({"path": doc})).unwrap();
    let (tx, rx) = channel();
    let app = FilmcraftApp::new(s).with_control(rx);
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).build_eframe(move |_cc| app);
    for _ in 0..4 {
        let ctx = h.ctx.clone();
        let mut raw = std::mem::take(h.input_mut());
        eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
        *h.input_mut() = raw;
        h.step();
    }
    h.state_mut().ui.export.parts.open = true;
    for _ in 0..3 {
        let ctx = h.ctx.clone();
        let mut raw = std::mem::take(h.input_mut());
        eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
        *h.input_mut() = raw;
        h.step();
    }
    {
        let a = h.state_mut();
        assert_eq!(a.ui.export.parts.bili_title, "上一场的标题", "旁车灌回弹窗");
        assert_eq!(a.ui.export.parts.bili_desc, "上一场的简介");
        assert_eq!(a.ui.export.parts.bili_tags, vec!["reaction".to_string()]);
        assert!(!a.ui.export.parts.bili_edited[0], "灌回不是用户编辑");
    }

    // 手改标题:整份回写旁车(简介/标签跟着落盘),路径还是原来那份。
    {
        let a = h.state_mut();
        a.ui.export.parts.bili_title = "手改标题".into();
        a.ui.export.parts.bili_edited[0] = true;
    }
    for _ in 0..3 {
        let ctx = h.ctx.clone();
        let mut raw = std::mem::take(h.input_mut());
        eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
        *h.input_mut() = raw;
        h.step();
    }
    let written: Value = serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    assert_eq!(written["title"], json!("手改标题"), "手改回写旁车");
    assert_eq!(written["description"], json!("上一场的简介"), "未动字段跟着落盘");
    assert_eq!(written["tags"], json!(["reaction"]));

    // 再重开一次:新 app 读到的是手改后的模板。
    let mut s2 = Session::default();
    s2.execute("file.open", json!({"path": doc})).unwrap();
    let app2 = FilmcraftApp::new(s2);
    let mut h2 = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).build_eframe(move |_cc| app2);
    for _ in 0..4 {
        let ctx = h2.ctx.clone();
        let mut raw = std::mem::take(h2.input_mut());
        eframe::App::raw_input_hook(h2.state_mut(), &ctx, &mut raw);
        *h2.input_mut() = raw;
        h2.step();
    }
    h2.state_mut().ui.export.parts.open = true;
    for _ in 0..3 {
        let ctx = h2.ctx.clone();
        let mut raw = std::mem::take(h2.input_mut());
        eframe::App::raw_input_hook(h2.state_mut(), &ctx, &mut raw);
        *h2.input_mut() = raw;
        h2.step();
    }
    assert_eq!(h2.state_mut().ui.export.parts.bili_title, "手改标题", "重启后仍是手改值");
}

/// 端到端:「整段 + 分P」同选只渲染一次——整段(字幕烧录恒开)完成后,
/// 分P由引擎从整段产物里流复制切出,分P文件、投稿模板与状态栏都如实
/// 收尾。源与产物都是真实编码的 mp4(切分要读真实样本)。
#[test]
fn whole_export_is_split_into_parts_without_a_second_render() {
    let isolated = std::env::temp_dir().join(format!("parts-e2e-biliup-{}.json", std::process::id()));

    let mut d = Driver::new("parts-split-e2e");
    let out = d.dir.join("out");
    std::fs::create_dir_all(&out).unwrap();

    // 先从 demo 序列导出 12 秒真实 mp4,作为可解码的源;再从它新建序列。
    // (每P的 UI 下限是 0.1 min = 6s;两P至少要 12s 源,末P才不会被并入。)
    let src = d.dir.join("src.mp4");
    d.exec(
        "export.queue.add",
        json!({"path": src.to_string_lossy(), "format": "h264", "range": "custom", "startSeconds": 0.0, "endSeconds": 12.0, "start": true, "wait": true}),
    );
    assert!(src.metadata().map(|m| m.len() > 0).unwrap_or(false), "the 12s source rendered");
    let imported = d.exec("file.import", json!({"paths": [src.to_string_lossy()]}));
    let item = imported["items"][0].as_u64().expect("imported item");
    d.exec("project.select", json!({"items": [item]}));
    d.exec("file.newSequenceFromClip", json!({}));
    d.frames(2);

    d.ok("ui.set", json!({"mode": "edit"}));
    d.frames(2);
    d.click("header.quickExport");
    d.click("quickExport.parts");
    {
        // 12 秒序列,每P 6 秒(UI 下限 0.1 min)→ 恰好两P。
        let p = &mut d.app().ui.export.parts;
        p.location = out.to_string_lossy().to_string();
        p.per_part_minutes = 0.1;
        p.cover_path = None;
        p.intro = None;
        p.intro_loaded_for = None;
        // 隔离 biliup 配置:合并不到就回落写独立 .biliup.json,不碰用户机器。
        p.biliup_config_override = Some(isolated.clone());
    }
    d.frames(2);
    d.click("partsExport.go");
    let (in_flight, open, status) = {
        let a = d.app();
        (a.ui.export.parts.in_flight, a.ui.export.parts.open, a.ui.status.clone())
    };
    assert!(in_flight, "导出已起飞,弹窗已关 (open={open} status={status})");

    let t0 = std::time::Instant::now();
    while d.app().ui.export.parts.in_flight {
        assert!(t0.elapsed().as_secs() < 180, "timed out waiting for export + split");
        d.frames(4);
    }

    // 分P从整段产物里切出来,与整段同目录;视频轨可再次被解复用。
    let planned: Vec<(String, String)> = d
        .app()
        .ui
        .export
        .parts
        .planned_parts
        .iter()
        .map(|p| (p.item.file.clone(), p.item.path.clone()))
        .collect();
    assert!(planned.len() >= 2, "two 1s parts were planned: {planned:?}");
    for (file, path) in &planned {
        assert!(Path::new(path).is_file(), "{file} was split out of the whole video");
    }
    // 整段在,收尾如实:3 个视频都成了;biliup 合并被隔离 → 报错回落
    // 独立文件(状态栏是那条错误,产物不丢)。
    let whole = &d.app().ui.export.parts.items[0].path;
    assert!(Path::new(whole).is_file(), "the whole render landed");
    assert!(d.app().ui.status.contains("biliup config.json not found"), "回落路径被走到: {}", d.app().ui.status);
    // 投稿模板回落成独立文件(biliup 配置被隔离),分P与整段都列进去。
    let base = d.app().ui.export.parts.base_name.clone();
    let template: Value = serde_json::from_slice(&std::fs::read(out.join(format!("{base}.biliup.json"))).unwrap()).unwrap();
    let videos: Vec<Value> = template["videos"].as_array().unwrap().clone();
    assert_eq!(videos.len(), 3, "whole + 2 parts: {videos:?}");
    for (_, path) in &planned {
        assert!(videos.iter().any(|v| v["path"].as_str() == Some(path.as_str())), "each part is listed");
    }
    let _ = std::fs::remove_file(&isolated);
}
