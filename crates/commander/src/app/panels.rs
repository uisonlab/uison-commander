//! Panels: reading folders, the cursor, the context menu, opening.

use super::*;

impl App {
    /// Read `dir` into panel `side` (in a worker). The panel switches to it when the listing arrives;
    /// `focus` — the name to put the cursor on.
    pub fn load(&mut self, side: usize, dir: VPath, focus: Option<String>) {
        let src = self.panels[side].src.clone();
        self.load_from(side, src, dir, focus);
    }

    /// `load` from another source: the panel keeps its source and folder until the listing arrives, so
    /// nothing acts on the old folder's path in the new source meanwhile — or ever, if the read fails.
    pub(super) fn load_from(&mut self, side: usize, src: Arc<dyn Source>, dir: VPath, focus: Option<String>) {
        let seq = self.next_seq();
        let p = &mut self.panels[side];
        if let Some(c) = p.loading.take() {
            c.cancel();
        }
        // Sizes counted in the folder being left are of no use.
        if dir != p.dir || !same_storage(&*src, &*p.src) {
            for c in p.sizing.drain(..) {
                c.cancel();
            }
        }
        let cancel = Cancel::new();
        p.loading = Some(cancel.clone());
        p.seq = seq;
        // A step through the history says so after this.
        p.going = None;
        let mb = self.mb.clone();
        // "Loading…" only for a slow read: a fast folder doesn't blink.
        let late = mb.clone();
        spawn(move || {
            std::thread::sleep(SLOW_LIST);
            late.send(Msg::Slow { side, seq });
        });
        spawn(move || {
            let res = cancel.scoped(|| src.list(&dir, &cancel));
            if res != Err(vfs::Error::Cancelled) {
                let space = res.is_ok().then(|| src.space(&dir)).flatten();
                mb.send(Msg::Listed { side, seq, src, dir, res, focus, space });
            }
        });
    }

    pub(super) fn refresh(&mut self, side: usize) {
        let dir = self.panels[side].dir.clone();
        self.load(side, dir, None);
    }

    /// Both panels, if they show something of `src` (after an operation).
    pub(super) fn refresh_all(&mut self) {
        for side in 0..2 {
            self.refresh(side);
        }
    }

    pub(super) fn write_panels(&self, d: &mut Data) {
        for (i, p) in self.panels.iter().enumerate() {
            p.write(d, i == self.active);
            self.write_menu(i, d);
        }
        d.set("active", if self.active == 0 { "left" } else { "right" });
    }

    pub(super) fn write_panel(&self, side: usize, d: &mut Data) {
        self.panels[side].write(d, side == self.active);
        self.write_menu(side, d);
    }

    /// The cursor moved: the cursor itself, the panel's menu, the scroll.
    pub(super) fn moved(&self, side: usize, cx: &mut Cx) {
        let p = &self.panels[side];
        p.write_cursor(cx.data);
        // With a selection the menu is about the selection: moving the cursor doesn't change it.
        if !p.has_selection() {
            self.write_menu(side, cx.data);
        }
        let r = p.cursor as f32;
        cx.scroll_into_view(&format!("{}-list", p.side), r * ROW_H, (r + 1.0) * ROW_H);
    }

    /// The context menu of a panel: standard items and the source's own actions for the selection.
    pub(super) fn write_menu(&self, side: usize, d: &mut Data) {
        let p = &self.panels[side];
        let ro = !p.src.info().caps.write;
        let item = |label: &str, action: &str, shortcut: &str, disabled: bool| {
            obj([
                ("label", Json::from(label.to_string())),
                ("action", Json::from(action.to_string())),
                ("shortcut", Json::from(shortcut.to_string())),
                ("disabled", Json::from(disabled)),
            ])
        };
        let mut items = vec![
            item(t!("Open"), "ctxOpen", "Enter", false),
            item(t!("View"), "ctxView", "F3", false),
            item(t!("Edit"), "ctxEdit", "F4", false),
            Json::from("-"),
            item(t!("Copy"), "ctxCopy", "F5", false),
            item(t!("Move"), "ctxMove", "F6", ro),
            item(t!("Rename"), "ctxRename", "Shift+F6", ro),
            item(t!("Delete"), "ctxDelete", "F8", ro),
            Json::from("-"),
            item(t!("Copy path"), "ctxPath", "⌥⌘C", false),
            item(t!("Properties"), "ctxProps", "Alt+Enter", false),
        ];
        let actions = p.src.actions(&p.dir, &p.targets());
        if !actions.is_empty() {
            items.push(Json::from("-"));
            for (i, a) in actions.iter().enumerate().take(ACTION_SLOTS) {
                let title = if a.params.is_empty() { t!(&a.title).to_string() } else { format!("{}…", t!(&a.title)) };
                items.push(item(&title, &format!("ctxAct{i}"), "", false));
            }
        }
        d.set(&format!("{}.menu", p.side), Json::Array(items));
    }

    pub(super) fn set_active(&mut self, side: usize, cx: &mut Cx) {
        if self.active != side {
            self.close_quick(cx);
            self.active = side;
            for (i, p) in self.panels.iter().enumerate() {
                p.write_status(cx.data, i == side);
            }
            cx.data.set("active", if side == 0 { "left" } else { "right" });
        }
    }

    pub(super) fn close_quick(&mut self, cx: &mut Cx) {
        let a = self.active;
        if self.panels[a].quick.take().is_some() {
            self.panels[a].write_cursor(cx.data);
        }
    }

    pub(super) fn other(&self) -> usize {
        1 - self.active
    }

    /// Enter: a folder opens in the panel, a file — with the system (local) or in the viewer.
    pub(super) fn open(&mut self, cx: &mut Cx) {
        let a = self.active;
        let p = &self.panels[a];
        if p.on_up() {
            self.parent(cx);
            return;
        }
        let Some(e) = p.current().cloned() else { return };
        if e.is_dir() {
            let dir = p.dir.child(&e.name);
            self.load(a, dir, None);
            return;
        }
        // A file on this machine opens with its app; elsewhere (a server, a bucket) in the viewer.
        match p.src.local_path(&p.dir.child(&e.name)) {
            Some(path) => cx.open_url(&file_url(&path.to_string_lossy())),
            None => self.view_file(cx),
        }
    }

    /// Alt+← / Alt+→: the previous folder of this panel, or the next one again.
    pub(super) fn go_history(&mut self, back: bool) {
        let a = self.active;
        let p = &self.panels[a];
        let Some((src, dir)) = (if back { p.back.last() } else { p.forward.last() }).cloned() else { return };
        let focus = (same_storage(&*src, &*p.src) && back).then(|| p.dir.strip_prefix(&dir)).flatten();
        let focus = focus.and_then(|rest| rest.split('/').next().filter(|n| !n.is_empty()).map(str::to_string));
        self.load_from(a, src, dir, focus);
        self.panels[a].going = Some(back);
    }

    pub(super) fn parent(&mut self, _cx: &mut Cx) {
        let a = self.active;
        let p = &self.panels[a];
        if let Some(up) = p.dir.parent() {
            let name = p.dir.name().to_string();
            self.load(a, up, Some(name));
        }
    }
}
