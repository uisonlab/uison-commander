//! Keys and the commands they run.

use super::*;

impl App {
    pub(super) fn key(&mut self, cx: &mut Cx) {
        let key = cx.key.to_string();
        // The settings panel: Escape closes it, other keys go on as usual.
        if self.settings && key == "escape" {
            return self.close_settings(cx);
        }
        if self.dlg.is_some() {
            self.dialog_key(&key, cx);
            return;
        }
        if self.view.is_some() {
            self.view_key(&key, cx);
            return;
        }
        if let Some(job) = &self.job
            && !job.background
        {
            if key == "escape" {
                job.cancel.cancel();
            }
            return;
        }
        let a = self.active;
        // Escape stops a folder that is still being read; the panel stays where it was.
        if key == "escape"
            && let Some(c) = self.panels[a].loading.take()
        {
            c.cancel();
            self.panels[a].write_status(cx.data, true);
            return;
        }
        // Quick search takes typing, Backspace and Escape while it is open.
        if self.panels[a].quick.is_some() {
            match key.as_str() {
                "escape" => return self.close_quick(cx),
                "backspace" => {
                    let p = &mut self.panels[a];
                    if let Some(q) = &mut p.quick {
                        q.pop();
                    }
                    p.write_cursor(cx.data);
                    return;
                }
                _ => {}
            }
        }
        let Some(c) = command(&key) else { return };
        if !matches!(c, Cmd::Search(_)) {
            self.close_quick(cx);
        }
        self.run(c, cx);
    }

    pub fn run(&mut self, c: Cmd, cx: &mut Cx) {
        let a = self.active;
        // Rows that fit, less the panel's header and footer; never less than one.
        let page = ((cx.data.num("env.height") as f32 / ROW_H) as isize - 6).max(1);
        match c {
            Cmd::Up | Cmd::Down | Cmd::PageUp | Cmd::PageDown | Cmd::Home | Cmd::End => {
                let p = &mut self.panels[a];
                let cur = p.cursor as isize;
                let to = match c {
                    Cmd::Up => cur - 1,
                    Cmd::Down => cur + 1,
                    Cmd::PageUp => cur - page,
                    Cmd::PageDown => cur + page,
                    Cmd::Home => 0,
                    _ => isize::MAX / 2,
                };
                p.move_cursor(to);
                self.moved(a, cx);
            }
            Cmd::Open => self.open(cx),
            Cmd::Parent => self.parent(cx),
            Cmd::Root => self.load(a, VPath::root(), None),
            Cmd::Back | Cmd::Forward => self.go_history(c == Cmd::Back),
            Cmd::SwitchPanel => self.set_active(self.other(), cx),
            Cmd::Toggle | Cmd::ToggleSize | Cmd::ToggleDown | Cmd::ToggleUp => {
                let p = &mut self.panels[a];
                if let Some(i) = p.toggle() {
                    p.write_row(cx.data, i);
                    let e = p.items[i].entry.clone();
                    if c == Cmd::ToggleSize && e.is_dir() && p.items[i].sel && !p.sizes.contains_key(&e.name) {
                        self.count_size(a, e.name);
                    }
                }
                let p = &mut self.panels[a];
                p.write_status(cx.data, true);
                // Insert, Space and Shift+Down go down after selecting, as in TC; Shift+Up goes up.
                let step = if c == Cmd::ToggleUp { -1 } else { 1 };
                if step != 0 {
                    let to = p.cursor as isize + step;
                    p.move_cursor(to);
                }
                self.moved(a, cx);
            }
            Cmd::SelectMask | Cmd::UnselectMask => {
                let select = c == Cmd::SelectMask;
                let title = if select { t!("Select") } else { t!("Unselect") };
                self.open_dialog(Dialog::input(title, t!("Mask (several with ';'):"), "*.*", t!("OK"), Act::Mask { side: a, select }), cx);
            }
            Cmd::Invert | Cmd::SelectAll | Cmd::UnselectAll => {
                let p = &mut self.panels[a];
                for it in &mut p.items {
                    // Folders are not inverted, as in TC.
                    it.sel = match c {
                        Cmd::Invert if it.entry.is_dir() => it.sel,
                        Cmd::Invert => !it.sel,
                        Cmd::SelectAll => true,
                        _ => false,
                    };
                }
                self.write_panel(a, cx.data);
            }
            Cmd::View => self.view_file(cx),
            Cmd::Edit => self.edit_file(),
            Cmd::Copy | Cmd::Move => self.ask_transfer(if c == Cmd::Copy { Mode::Copy } else { Mode::Move }, false, cx),
            Cmd::CopyHere => self.ask_transfer(Mode::Copy, true, cx),
            Cmd::Rename => self.ask_transfer(Mode::Move, true, cx),
            Cmd::Mkdir => {
                if self.writable(a, cx) {
                    self.open_dialog(
                        Dialog::input(t!("New folder"), t!("Name (a/b/c makes all levels):"), "", t!("Create"), Act::Mkdir { side: a }),
                        cx,
                    );
                }
            }
            Cmd::NewFile => {
                if self.writable(a, cx) {
                    self.open_dialog(Dialog::input(t!("New file"), t!("Name:"), "", t!("Create"), Act::NewFile { side: a }), cx);
                }
            }
            // The menu under the button: a new connection, or the remembered ones.
            Cmd::Connect => {
                if !crate::connect::all().is_empty() {
                    self.close_settings(cx);
                    cx.context_menu("tb-connect");
                }
            }
            Cmd::Delete => self.ask_delete(cx),
            Cmd::PickSource(side) => self.pick_source(side, cx),
            Cmd::Sort(k) => self.sort(a, k, cx),
            Cmd::Refresh => self.refresh(a),
            Cmd::Swap => {
                self.panels.swap(0, 1);
                self.panels[0].side = "left";
                self.panels[1].side = "right";
                // What is on its way addresses panels by side: read both again; the operation follows too.
                if let Some(j) = &mut self.job {
                    j.focus = j.focus.take().map(|(s, n)| (1 - s, n));
                    j.deselect = j.deselect.map(|s| 1 - s);
                }
                self.write_panels(cx.data);
                self.refresh_all();
            }
            Cmd::OpenInOther => {
                let p = &self.panels[a];
                let dir = match p.current() {
                    Some(e) if e.is_dir() => p.dir.child(&e.name),
                    _ => p.dir.clone(),
                };
                let src = p.src.clone();
                let o = self.other();
                self.load_from(o, src, dir, None);
            }
            Cmd::Properties => self.properties(cx),
            Cmd::Menu => {
                let p = &self.panels[a];
                cx.context_menu(&format!("{}-r{}", p.side, p.cursor));
            }
            Cmd::Hidden => {
                for p in &mut self.panels {
                    p.show_hidden = !p.show_hidden;
                    p.rebuild(None);
                }
                self.write_panels(cx.data);
            }
            Cmd::Help => self.help(cx),
            Cmd::CopyPaths | Cmd::CopyNames => {
                let p = &self.panels[a];
                let targets = p.targets();
                if targets.is_empty() {
                    return;
                }
                // A file on the disk by its own path; elsewhere the source's name first (`sftp:u@h:/home/u/x`).
                let lines: Vec<String> = targets
                    .iter()
                    .map(|e| {
                        let path = p.dir.child(&e.name);
                        match (&c, p.src.local_path(&path)) {
                            (Cmd::CopyNames, _) => e.name.clone(),
                            (_, Some(os)) => os.to_string_lossy().into_owned(),
                            (_, None) => format!("{}:{path}", p.src.info().title),
                        }
                    })
                    .collect();
                cx.copy(&lines.join("\n"));
                let what = if c == Cmd::CopyNames { "name" } else { "path" };
                self.toast(&t!("Copied: {}", format::count(lines.len() as u64, what)), cx);
            }
            Cmd::Quit if self.job.is_some() => {
                let d = Dialog::new(Kind::Message, t!("Quit"), t!("An operation is running. Cancel it and quit?"))
                    .buttons(vec![Button::new(t!("Quit"), "danger", Act::QuitNow), Button::cancel()]);
                self.open_dialog(d, cx);
            }
            Cmd::Quit => self.quit(),
            Cmd::Search(t) => {
                let p = &mut self.panels[a];
                let q = format!("{}{t}", p.quick.clone().unwrap_or_default());
                // A prefix that matches nothing is not taken (TC beeps).
                if let Some(r) = p.quick_find(&q) {
                    p.quick = Some(q);
                    p.cursor = r;
                } else if p.quick.is_none() {
                    p.quick = Some(String::new());
                }
                self.moved(a, cx);
            }
        }
    }

    /// Help → About: the version and the website (a link on the card).
    pub(super) fn about(&mut self, cx: &mut Cx) {
        let d = Dialog::new(Kind::About, "uison-commander", "").buttons(vec![Button::new(t!("Close"), "secondary", Act::Close)]);
        self.open_dialog(d, cx);
    }

    /// F1: every key, grouped, straight from the key table.
    pub(super) fn help(&mut self, cx: &mut Cx) {
        let mut items = Vec::new();
        let mut group = "";
        for b in crate::keymap::table().into_iter().filter(|b| !b.help.is_empty()) {
            if b.group != group {
                group = b.group;
                items.push(ListItem { label: t!(group).into(), note: String::new(), source: 0, path: VPath::root(), profile: None, header: true });
            }
            let keys: Vec<String> = b.keys.iter().map(|k| crate::keymap::key_title(k)).collect();
            items
                .push(ListItem { label: t!(b.help).into(), note: keys.join("  ·  "), source: 0, path: VPath::root(), profile: None, header: false });
        }
        let text = if cfg!(target_os = "macos") { t!("⌘ works as ⌃ (Ctrl) everywhere, except ⌘Q and ⌘⌫.") } else { "" };
        let mut d = Dialog::new(Kind::List, t!("Keys"), text).buttons(vec![Button::new(t!("Close"), "primary", Act::Close)]);
        d.sel = items.iter().position(|it| !it.header).unwrap_or(0);
        d.items = items;
        d.wide = true;
        self.open_dialog(d, cx);
    }

    pub(super) fn sort(&mut self, side: usize, k: SortKey, cx: &mut Cx) {
        let p = &mut self.panels[side];
        // The same key again reverses the order.
        p.sort = Sort { key: k, desc: p.sort.key == k && !p.sort.desc };
        p.rebuild(None);
        self.write_panel(side, cx.data);
        self.moved(side, cx);
    }

    pub(super) fn writable(&mut self, side: usize, cx: &mut Cx) -> bool {
        let src = &self.panels[side].src;
        if src.info().caps.write {
            return true;
        }
        let msg = t!("{} is read-only.", src.info().title);
        self.open_dialog(Dialog::message(t!("Read-only"), &msg), cx);
        false
    }
}
