use super::{FilmcraftApp, MENU_SYNC_INTERVAL, menus, state};
#[cfg(test)]
use filmcraft_engine::Session;
#[cfg(test)]
use serde_json::json;

impl FilmcraftApp {
    // ---------------------------------------------------------------- input

    /// The pass's mode switch, if any: leaving Reaction mode hands the native menu bar back
    /// (macOS) — the embedded reaction replaced it while that page drew, and nothing else restores
    /// ours. Entering it, and any switch between the other modes, restores nothing. Every pass
    /// also syncs the reaction engine's acquisition switch with the page's visibility.
    pub(super) fn note_mode_change(&mut self) {
        if self.menu_mode != self.ui.mode {
            let left_reaction = self.menu_mode == state::Mode::Reaction;
            self.menu_mode = self.ui.mode;
            if left_reaction && let Some(restore) = self.hooks.restore_menu.as_mut() {
                restore();
            }
        }
        self.sync_reaction_engine();
    }

    /// Reaction 页在不在屏幕上,决定内嵌实例的引擎要不要采集。页面一走
    /// 就整个关掉——桌面/摄像头采集替一个没人看的画布跑是纯开销,而且
    /// 系统的屏幕录制/摄像头指示灯也一直亮着——回到本页(或实例忙起
    /// 来:录制/推流/虚拟摄像头靠采集活着)再按来源清单重开。实例本身
    /// 不拆:录后 AI 元数据分钟级才经它交付,宿主每帧在等它。只在状态
    /// 变化时发命令。
    pub(super) fn sync_reaction_engine(&mut self) {
        let active = match self.reaction.as_ref() {
            Some(reaction) => self.ui.mode == state::Mode::Reaction || reaction.is_busy(),
            None => return,
        };
        if active == self.reaction_engine_active {
            return;
        }
        if let Some(reaction) = self.reaction.as_mut() {
            reaction.set_engine_active(active);
        }
        self.reaction_engine_active = active;
    }

    /// Sends the menu tree to the native menu bar when it changed, so its items show the same
    /// labels, shortcuts and enablement as the in-window menus (Undo with nothing to undo is
    /// disabled). The items a focused text field takes stay enabled while one has the keyboard.
    /// Without a native menu bar this does nothing.
    pub(super) fn sync_native_menu(&mut self, ctx: &egui::Context) {
        if self.hooks.menu_changed.is_none() {
            return;
        }
        // A background window draws no menu bar of this application's, so
        // there is nothing to keep current — one pass after it comes back
        // sends whatever changed meanwhile. A minimised one is the same.
        let focused = ctx.input(|input| input.viewport().focused);
        let minimized = ctx.input(|input| input.viewport().minimized.unwrap_or(false));
        if focused == Some(false) || minimized {
            return;
        }
        let now = ctx.input(|input| input.time);
        // Only when the clock moved: the tests below call this repeatedly in
        // one egui instant and expect every call to take effect.
        if let Some(at) = self.menu_sent_at
            && now > at
            && now - at < MENU_SYNC_INTERVAL
        {
            return;
        }
        let mut items = menus::menu_items(self);
        if menus::text_field_window(ctx).is_some() {
            for it in items.iter_mut().filter(|it| menus::edits_text(&it.id)) {
                it.enabled = true;
            }
        } else if self.ui.gfx_edit.is_some() {
            // a graphic's text: its Copy, Cut, Paste and Select All, whatever the project allows (#1953)
            for it in items.iter_mut().filter(|it| menus::edits_graphic_text(&it.id)) {
                it.enabled = true;
            }
        }
        self.menu_sent_at = Some(now);
        if items == self.menu_sent {
            return;
        }
        if let Some(hook) = self.hooks.menu_changed.as_mut() {
            hook(&items);
        }
        self.menu_sent = items;
    }
}

impl FilmcraftApp {
    /// Drain native menu/control command ids, preserving the control-only Quit exception.
    pub(super) fn logic_command_inbox(&mut self, ctx: &egui::Context) {
        if let Some(rx) = self.command_inbox.take() {
            while let Ok(id) = rx.try_recv() {
                // control-only mode ignores native menu commands, but Quit stays available (#1943)
                if self.control_input_only && id != "app.quit" {
                    continue;
                }
                if let Err(e) = menus::activate_native(self, ctx, &id) {
                    self.ui.status = e;
                }
            }
            self.command_inbox = Some(rx);
        }
    }
}

#[cfg(test)]
mod native_menu_tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn enabled(items: &[menus::MenuItem], id: &str) -> Option<bool> {
        items.iter().find(|it| it.id == id).map(|it| it.enabled)
    }

    /// The native menu bar (macOS) gets the menu tree whenever it changes, so Edit ▸ Undo and Redo
    /// follow the history instead of staying enabled (#902).
    #[test]
    fn native_menu_follows_undo_and_redo_availability() {
        let sent: Rc<RefCell<Vec<Vec<menus::MenuItem>>>> = Rc::default();
        let mut app = FilmcraftApp::new(Session::default());
        let sink = sent.clone();
        app.hooks.menu_changed = Some(Box::new(move |items: &[menus::MenuItem]| sink.borrow_mut().push(items.to_vec())));
        let ctx = egui::Context::default();

        app.sync_native_menu(&ctx);
        let last = |sent: &Rc<RefCell<Vec<Vec<menus::MenuItem>>>>, id: &str| sent.borrow().last().and_then(|items| enabled(items, id));
        assert_eq!(last(&sent, "edit.undo"), Some(false), "nothing to undo in an empty project");
        assert_eq!(last(&sent, "edit.redo"), Some(false), "nothing to redo in an empty project");
        assert_eq!(last(&sent, "file.newProject"), Some(true));

        app.sync_native_menu(&ctx);
        assert_eq!(sent.borrow().len(), 1, "an unchanged menu is not sent again");

        app.session.execute("file.newSequence", json!({"width":16,"height":16})).unwrap();
        app.sync_native_menu(&ctx);
        assert_eq!(last(&sent, "edit.undo"), Some(true), "an edit can be undone");
        assert_eq!(last(&sent, "edit.redo"), Some(false));

        app.session.execute("edit.undo", json!({})).unwrap();
        app.sync_native_menu(&ctx);
        assert_eq!(last(&sent, "edit.undo"), Some(false));
        assert_eq!(last(&sent, "edit.redo"), Some(true), "the undone edit can be redone");
    }

    /// #1953: AppKit does not send a disabled item's command, so while a graphic's text is edited
    /// its Copy, Cut, Paste and Select All stay enabled with no clip selected or copied; Undo stays
    /// the project's.
    #[test]
    fn native_edit_items_stay_enabled_while_a_graphic_text_is_edited() {
        let sent: Rc<RefCell<Vec<Vec<menus::MenuItem>>>> = Rc::default();
        let mut app = FilmcraftApp::new(Session::default());
        let sink = sent.clone();
        app.hooks.menu_changed = Some(Box::new(move |items: &[menus::MenuItem]| sink.borrow_mut().push(items.to_vec())));
        let ctx = egui::Context::default();
        let last = |sent: &Rc<RefCell<Vec<Vec<menus::MenuItem>>>>, id: &str| sent.borrow().last().and_then(|items| enabled(items, id));
        app.sync_native_menu(&ctx);
        assert_eq!(last(&sent, "edit.copy"), Some(false), "nothing selected to copy");
        app.ui.gfx_edit = Some(crate::state::GfxEdit::default());
        app.sync_native_menu(&ctx);
        for id in ["edit.copy", "edit.cut", "edit.paste", "edit.selectAll"] {
            assert_eq!(last(&sent, id), Some(true), "{id} while a graphic's text is edited");
        }
        assert_eq!(last(&sent, "edit.undo"), Some(false), "Undo stays the project's");
    }

    /// Leaving Reaction mode tells the host to put this application's menus back in the menu bar
    /// (the embedded reaction replaced them while that page drew). Entering it must not, and a
    /// change between the other modes must not either.
    #[test]
    fn leaving_reaction_mode_restores_the_native_menu() {
        let restored: Rc<RefCell<u32>> = Rc::default();
        let mut app = FilmcraftApp::new(Session::default());
        let sink = restored.clone();
        app.hooks.restore_menu = Some(Box::new(move || *sink.borrow_mut() += 1));

        app.ui.mode = state::Mode::Reaction;
        app.note_mode_change();
        assert_eq!(*restored.borrow(), 0, "entering Reaction leaves the menu bar to reaction");

        app.ui.mode = state::Mode::Home;
        app.note_mode_change();
        assert_eq!(*restored.borrow(), 1, "leaving Reaction hands the menu bar back");

        app.ui.mode = state::Mode::Edit;
        app.note_mode_change();
        assert_eq!(*restored.borrow(), 1, "a change that does not leave Reaction restores nothing");

        app.note_mode_change();
        assert_eq!(*restored.borrow(), 1, "no mode change, no restore");
    }
}
