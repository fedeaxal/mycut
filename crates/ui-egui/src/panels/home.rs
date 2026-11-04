//! Home mode: the project wall. Reached through the control API (`mode: "home"`); the header's
//! tabs offer Reaction / Import / Edit / Export.
//!
//! The merged application's front door. Up top, two doors out — **Reaction**
//! opens the reaction recorder as a sibling program (this window steps aside,
//! and comes back when it closes); **New edit** creates a filmcraft project
//! right here. Below, a cover per edit project — the project's frame 0,
//! rendered by `filmcraft-cli` in the background — clicking one opens it in
//! this same window.
//!
//! The list of edits lives in a file this page owns
//! (`<data dir>/studio/projects.toml`); deleting a project removes its
//! entry. Directory scanning is deliberately not a thing: the file is the
//! list.
//!
//! This page came from Studio's launcher, which it replaces.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::SystemTime;

use egui::{Align2, Color32, Rect, Sense, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::FilmcraftApp;
use crate::theme::Tokens;

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    ui.painter().rect_filled(rect, t.radius, t.panel_bg);

    // The wall reads and writes its own state through `app.ui.home`; the
    // draws below take plain values so no borrow crosses a widget.
    app.ui.home.drain_messages(ui.ctx());
    app.ui.home.refresh_covers(ui.ctx());

    let rx = rect.min.x;
    // The two doors, top left.
    ui.painter().text(rect.min + vec2(24.0, 40.0), Align2::LEFT_CENTER, tl!("Home"), Tokens::semibold(22.0), t.text);
    let mut cursor = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(pos2(rx + 24.0, rect.min.y + 60.0), pos2(rect.max.x - 24.0, rect.max.y))));
    cursor.horizontal(|ui| {
        let mut button = |label: &str| ui.add(egui::Button::new(egui::RichText::new(label).size(17.0)).min_size(vec2(150.0, 38.0)));
        if button(tl!("Reaction")).clicked() {
            // 模式切换,而非启动另一个程序:reaction 内嵌在本窗口运行,
            // 顶栏的 Reaction 页签同为此路。
            app.ui.mode = crate::state::Mode::Reaction;
        }
        if button(tl!("New edit")).clicked() {
            app.ui.home.new_open = true;
        }
        if !app.ui.home.status.is_empty() {
            ui.weak(&app.ui.home.status);
        }
    });
    cursor.add_space(18.0);
    project_wall(app, &mut cursor, rect);
    new_edit_dialog(app, ui.ctx());
    delete_dialog(app, ui.ctx());
}

// ---------------------------------------------------------------- config --

/// One edit project the wall knows about.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ProjectEntry {
    /// Stable name for the thumbnail file, minted at creation.
    id: String,
    name: String,
    /// The project file, absolute.
    path: PathBuf,
    /// When the project was created, as a Unix epoch second — read by a
    /// person, sorted by nothing.
    created_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProjectsConfig {
    projects: Vec<ProjectEntry>,
}

/// This user's Studio data directory — the same place the launcher used to
/// keep it, so walls made before the merge carry over.
fn data_dir() -> PathBuf {
    let home = |name: &str| std::env::var(name).ok().map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = home("HOME").map(|h| h.join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let base = home("APPDATA");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = home("XDG_DATA_HOME").or_else(|| home("HOME").map(|h| h.join(".local/share")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("studio")
}

fn config_path() -> PathBuf {
    data_dir().join("projects.toml")
}

fn thumbs_dir() -> PathBuf {
    data_dir().join("thumbs")
}

fn thumb_path(id: &str) -> PathBuf {
    thumbs_dir().join(format!("{id}.png"))
}

/// Where new projects go. Created on first use.
fn projects_dir() -> PathBuf {
    let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."));
    home.join("Documents").join("Studio Projects")
}

impl ProjectsConfig {
    fn load() -> Self {
        std::fs::read_to_string(config_path()).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default()
    }

    fn save(&self) {
        let Ok(text) = toml::to_string_pretty(self) else { return };
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(config_path(), text);
    }
}

/// The built sibling binary: `<exe dir>/<name>`, which is where everything
/// in the workspace lands — `filmcraft/target/<profile>/`.
fn sibling_binary(bin: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let file = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
    let candidate = exe.parent()?.join(file);
    candidate.is_file().then_some(candidate)
}

/// Whether the rendered thumbnail is at least as new as the project file.
fn thumbnail_is_fresh(entry: &ProjectEntry) -> bool {
    let Ok(project) = std::fs::metadata(&entry.path) else {
        return thumb_path(&entry.id).exists();
    };
    let Ok(thumb) = std::fs::metadata(thumb_path(&entry.id)) else { return false };
    match (project.modified(), thumb.modified()) {
        (Ok(project), Ok(thumb)) => thumb >= project,
        _ => false,
    }
}

/// A PNG's pixels as RGBA8, what `ColorImage` takes.
fn decode_png(bytes: &[u8]) -> Option<(Vec<u8>, [usize; 2])> {
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let size = [info.width as usize, info.height as usize];
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.as_chunks::<3>().0.iter().flat_map(|px| [px[0], px[1], px[2], 255]).collect(),
        _ => return None,
    };
    Some((rgba, size))
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A fresh entry id: enough nanoseconds that two projects created in the
/// same second still differ.
fn new_id() -> String {
    format!("p{}", SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0))
}

/// `name.fcproj`, or the first of `name-2.fcproj`, `name-3.fcproj`… not taken.
fn unique_project_path(dir: &Path, name: &str) -> PathBuf {
    let base = dir.join(format!("{name}.fcproj"));
    if !base.exists() {
        return base;
    }
    for n in 2..1000 {
        let candidate = dir.join(format!("{name}-{n}.fcproj"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{name}-{}.fcproj", now_unix()))
}

/// The file name part of a path built here, which always has one — but the
/// type cannot know, and the callers join it back rather than carry an Option.
fn file_name_of(path: &Path) -> PathBuf {
    path.file_name().map(PathBuf::from).unwrap_or(path.into())
}

// ----------------------------------------------------------------- state --

/// What a background thread reports back to the interface.
enum Message {
    /// A cover render finished, well or badly.
    ThumbDone(String),
    /// A new project's file was written, or the attempt failed.
    Created { id: String, ok: bool },
}

/// One cover render in flight at once — each is a headless engine start.
const MAX_CONCURRENT_RENDERS: usize = 2;
const CARD_SIZE: egui::Vec2 = egui::vec2(224.0, 126.0);

/// The wall's own state, alive as long as the interface is.
///
/// Not `Clone`/`Debug` by derive — the channel and the running program are
/// one session's, and a clone of the wall would not share them. The trait
/// impls the `UiState` derives require copy the rest and leave these out.
#[derive(Default)]
pub struct HomeState {
    covers: HashMap<String, egui::TextureHandle>,
    rendering: HashSet<String>,
    tx: Option<mpsc::Sender<Message>>,
    rx: Option<Receiver<Message>>,
    new_open: bool,
    new_name: String,
    delete_target: Option<String>,
    status: String,
}

impl Clone for HomeState {
    fn clone(&self) -> Self {
        Self {
            covers: self.covers.clone(),
            rendering: self.rendering.clone(),
            tx: self.tx.clone(),
            rx: None, // a clone of the wall does not inherit the channel
            new_open: self.new_open,
            new_name: self.new_name.clone(),
            delete_target: self.delete_target.clone(),
            status: self.status.clone(),
        }
    }
}

impl std::fmt::Debug for HomeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeState")
            .field("rendering", &self.rendering.len())
            .field("covers", &self.covers.len())
            .field("new_open", &self.new_open)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl HomeState {
    fn channel(&mut self) -> Option<mpsc::Sender<Message>> {
        if self.rx.is_none() {
            let (tx, rx) = mpsc::channel();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
        self.tx.clone()
    }
}

// ------------------------------------------------------------------ wall --

fn project_wall(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let projects = app.ui.home.load_projects();
    if projects.is_empty() {
        ui.add_space(ui.available_height() / 3.0);
        ui.vertical_centered(|ui| {
            ui.weak(tl!("No edit projects yet"));
            ui.weak(tl!("“New edit” up top creates the first one"));
        });
        return;
    }
    let columns = ((ui.available_width() / (CARD_SIZE.x + 24.0)) as usize).max(1);
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("studio_projects").min_col_width(CARD_SIZE.x).spacing(egui::vec2(20.0, 20.0)).show(ui, |ui| {
            let total = projects.len();
            for (n, entry) in projects.iter().enumerate() {
                project_card(app, ui, entry, rect);
                if (n + 1) % columns == 0 || n + 1 == total {
                    ui.end_row();
                }
            }
        });
    });
}

fn project_card(app: &mut FilmcraftApp, ui: &mut egui::Ui, entry: &ProjectEntry, _rect: Rect) {
    let gone = !entry.path.exists();
    let texture = app.ui.home.covers.get(&entry.id).cloned();
    let rendering = app.ui.home.rendering.contains(&entry.id);
    ui.vertical(|ui| {
        let (rect, response) = ui.allocate_exact_size(CARD_SIZE, Sense::click());
        if ui.is_rect_visible(response.rect) {
            let painter = ui.painter();
            let fill = if gone { ui.visuals().widgets.inactive.bg_fill } else { ui.visuals().extreme_bg_color };
            painter.rect_filled(rect, 6.0, fill);
            painter.rect_stroke(rect, 6.0, ui.visuals().widgets.inactive.bg_stroke, egui::StrokeKind::Inside);
            if let Some(texture) = texture {
                let size = texture.size_vec2();
                let scale = (rect.width() / size.x).min(rect.height() / size.y);
                let draw = egui::Rect::from_center_size(rect.center(), size * scale);
                painter.image(texture.id(), draw, egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            } else if gone {
                painter.text(rect.center(), Align2::CENTER_CENTER, tl!("Project file is gone"), Tokens::ui(11.0), ui.visuals().weak_text_color());
            } else if rendering {
                painter.text(rect.center(), Align2::CENTER_CENTER, tl!("Rendering the cover…"), Tokens::ui(11.0), ui.visuals().weak_text_color());
            }
            if response.clicked() && !gone {
                open_project(app, entry);
            }
            if response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        ui.horizontal(|ui| {
            ui.add_sized([CARD_SIZE.x - 30.0, 20.0], egui::Label::new(egui::RichText::new(&entry.name).strong()).truncate());
            if ui.add(egui::Button::new(egui::RichText::new("✕").small()).frame(false)).on_hover_text(tl!("Delete this project")).clicked() {
                app.ui.home.delete_target = Some(entry.id.clone());
            }
        });
        ui.add_sized([CARD_SIZE.x, 16.0], egui::Label::new(egui::RichText::new(entry.path.display().to_string()).weak()).truncate());
    });
}

/// Opens the project in this window — the wall's way into the editor.
fn open_project(app: &mut FilmcraftApp, entry: &ProjectEntry) {
    match app.session.execute("file.open", serde_json::json!({ "path": entry.path })) {
        Ok(_) => {
            app.ui.mode = crate::state::Mode::Edit;
            app.ui.home.status = format!("opened「{}」", entry.name);
        }
        Err(e) => app.ui.home.status = format!("could not open「{}」: {e}", entry.name),
    }
}

/// Loads the list, reads it once per pass (a small file), and queues the
/// covers it is missing.
impl HomeState {
    fn load_projects(&mut self) -> Vec<ProjectEntry> {
        let config = ProjectsConfig::load();
        for entry in config.projects.clone() {
            self.queue_cover(&entry);
        }
        config.projects
    }

    /// Sends `entry`'s cover to a background render unless one is already
    /// out for it or the queue is full.
    fn queue_cover(&mut self, entry: &ProjectEntry) {
        if self.rendering.contains(&entry.id) || self.rendering.len() >= MAX_CONCURRENT_RENDERS {
            return;
        }
        if !entry.path.exists() || thumbnail_is_fresh(entry) {
            return;
        }
        let Some(cli) = sibling_binary("filmcraft-cli") else { return };
        let project = entry.path.clone();
        let out = thumb_path(&entry.id);
        let _ = std::fs::create_dir_all(thumbs_dir());
        let id = entry.id.clone();
        let Some(tx) = self.channel() else { return };
        self.rendering.insert(id.clone());
        std::thread::spawn(move || {
            let _ = std::fs::remove_file(&out);
            // The project's frame 0, at half size — a card is 224 wide, the
            // render 960.
            let ok = std::process::Command::new(&cli)
                .args(["--project"])
                .arg(&project)
                .args(["render", "--seconds", "0", "--out"])
                .arg(&out)
                .args(["--scale", "0.5"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            let _ = tx.send(Message::ThumbDone(if ok { id } else { format!("{id}?failed") }));
        });
    }

    /// What arrived from the background threads since the last frame.
    fn drain_messages(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.rx.take() else { return };
        let drained = loop {
            match rx.try_recv() {
                Ok(Message::ThumbDone(id)) => {
                    let failed = id.ends_with("?failed");
                    let id = id.trim_end_matches("?failed");
                    self.rendering.remove(id);
                    if failed {
                        let _ = std::fs::remove_file(thumb_path(id));
                    }
                    self.covers.remove(id);
                    ctx.request_repaint();
                }
                Ok(Message::Created { id, ok }) => {
                    if ok {
                        self.status = "project created".into();
                        if let Some(entry) = ProjectsConfig::load().projects.into_iter().find(|p| p.id == id) {
                            self.queue_cover(&entry);
                        }
                    } else {
                        let mut config = ProjectsConfig::load();
                        config.projects.retain(|p| p.id != id);
                        config.save();
                        self.status = "creating the project failed".into();
                    }
                    ctx.request_repaint();
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break true,
            }
        };
        self.rx = Some(rx);
        if !drained {
            ctx.request_repaint();
        }
    }

    /// Loads covers whose file is there and current.
    fn refresh_covers(&mut self, ctx: &egui::Context) {
        for entry in ProjectsConfig::load().projects {
            if self.covers.contains_key(&entry.id) || self.rendering.contains(&entry.id) {
                continue;
            }
            if !entry.path.exists() {
                continue;
            }
            if !thumbnail_is_fresh(&entry) {
                self.queue_cover(&entry);
                continue;
            }
            if let Ok(bytes) = std::fs::read(thumb_path(&entry.id))
                && let Some((rgba, size)) = decode_png(&bytes)
            {
                let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                let handle = ctx.load_texture(&entry.id, image, egui::TextureOptions::LINEAR);
                self.covers.insert(entry.id.clone(), handle);
            }
        }
    }
}

// --------------------------------------------------------------- dialogs --

/// Creates the project file through the live session — one project and its
/// first sequence — records it, and renders its cover in the background.
fn create_project(app: &mut FilmcraftApp) {
    use crate::i18n::t;
    let name = app.ui.home.new_name.trim().to_string();
    if name.is_empty() {
        return;
    }
    let dir = projects_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        app.ui.home.status = format!("could not create {}: {e}", dir.display());
        return;
    }
    let file = unique_project_path(&dir, &name);
    let path = dir.join(file_name_of(&file));
    let entry = ProjectEntry { id: new_id(), name: name.clone(), path, created_at: now_unix() };
    let mut config = ProjectsConfig::load();
    config.projects.push(entry.clone());
    config.save();
    app.ui.home.new_open = false;
    app.ui.home.new_name.clear();
    app.ui.home.status = format!("creating「{name}」…");
    let created = [
        app.session.execute("file.newProject", serde_json::json!({ "name": name })).is_ok(),
        app.session.execute("file.newSequence", serde_json::json!({ "name": name, "width": 1920, "height": 1080, "fps": 30.0 })).is_ok(),
        app.session.execute("file.saveAs", serde_json::json!({ "path": entry.path })).is_ok(),
    ];
    let ok = created.iter().all(|&ok| ok);
    if !ok {
        let mut config = ProjectsConfig::load();
        config.projects.retain(|p| p.id != entry.id);
        config.save();
        app.ui.home.status = t("creating the project failed").to_string();
        return;
    }
    app.ui.home.status = format!("created「{}」", entry.name);
    let Some(tx) = app.ui.home.channel() else { return };
    let _ = tx.send(Message::Created { id: entry.id.clone(), ok: true });
    app.ui.home.queue_cover(&entry);
}

fn new_edit_dialog(app: &mut FilmcraftApp, ctx: &egui::Context) {
    if !app.ui.home.new_open {
        return;
    }
    let mut open = app.ui.home.new_open;
    egui::Window::new(tl!("New edit")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
        ui.label(tl!("Project name"));
        let name_field = ui.add(egui::TextEdit::singleline(&mut app.ui.home.new_name).desired_width(280.0).hint_text(tl!("For example: episode 12 reaction")));
        ui.add_space(4.0);
        ui.weak(format!("{}: {}", tl!("Location"), projects_dir().display()));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let pressed = ui.button(tl!("Create")).clicked();
            let enter = name_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (pressed || enter) && !app.ui.home.new_name.trim().is_empty() {
                create_project(app);
            }
            if ui.button(tl!("Cancel")).clicked() {
                app.ui.home.new_open = false;
                app.ui.home.new_name.clear();
            }
        });
    });
    app.ui.home.new_open = open;
    if app.ui.home.new_open {
        ctx.request_repaint();
    }
}

fn delete_dialog(app: &mut FilmcraftApp, ctx: &egui::Context) {
    let Some(id) = app.ui.home.delete_target.clone() else { return };
    let Some(entry) = ProjectsConfig::load().projects.into_iter().find(|p| p.id == id) else {
        app.ui.home.delete_target = None;
        return;
    };
    let mut open = true;
    egui::Window::new(tl!("Delete project")).open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
        ui.label(format!("{}「{}」?", tl!("Delete"), entry.name));
        ui.weak(tl!("The project file and its cover go with it. This cannot be undone."));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(egui::RichText::new(tl!("Delete")).color(Color32::RED)).clicked() {
                delete_project(app, &id);
                app.ui.home.delete_target = None;
            }
            if ui.button(tl!("Cancel")).clicked() {
                app.ui.home.delete_target = None;
            }
        });
    });
    if !open {
        app.ui.home.delete_target = None;
    }
}

/// Removes the record, the cover and the project file itself. A project
/// already gone is quietly removed; a project that refuses to be deleted
/// keeps its record, because it is still there.
fn delete_project(app: &mut FilmcraftApp, id: &str) {
    let mut config = ProjectsConfig::load();
    let Some(index) = config.projects.iter().position(|p| p.id == id) else { return };
    let entry = config.projects.remove(index);
    match std::fs::remove_file(&entry.path) {
        Ok(()) => app.ui.home.status = format!("deleted「{}」", entry.name),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => app.ui.home.status = format!("removed「{}」(the file was already gone)", entry.name),
        Err(e) => {
            config.projects.insert(index, entry);
            app.ui.home.status = format!("could not delete the project file: {e}");
        }
    }
    let _ = std::fs::remove_file(thumb_path(id));
    app.ui.home.covers.remove(id);
    config.save();
}
