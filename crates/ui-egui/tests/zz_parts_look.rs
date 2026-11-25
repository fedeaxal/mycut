//! 临时目测:分P导出弹窗底栏(主次按钮)(跑完即删)。

use std::path::Path;
use std::sync::mpsc::channel;

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::json;

fn step(h: &mut Harness<'static, FilmcraftApp>) {
    let ctx = h.ctx.clone();
    let mut raw = std::mem::take(h.input_mut());
    eframe::App::raw_input_hook(h.state_mut(), &ctx, &mut raw);
    *h.input_mut() = raw;
    h.step();
}

#[test]
fn parts_footer_look() {
    let mut s = Session::default();
    s.execute("file.openDemoProject", json!({})).unwrap();
    let (_tx, rx) = channel::<ControlRequest>();
    let app = FilmcraftApp::new(s).with_control(rx);
    let mut h = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).build_eframe(move |_cc| app);
    for _ in 0..4 {
        step(&mut h);
    }
    {
        let a = h.state_mut();
        a.ui.export.parts.open = true;
        a.ui.export.parts.whole_enabled = true;
    }
    for _ in 0..4 {
        step(&mut h);
    }
    let img = h.render().expect("wgpu render");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/export-tests/look");
    std::fs::create_dir_all(&dir).unwrap();
    img.save(dir.join("parts-footer.png")).unwrap();
}
