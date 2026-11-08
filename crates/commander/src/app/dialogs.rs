//! Dialogs: opening, the queue behind a question, notices, buttons; the source list (Alt+F1/F2) and the help.

use super::*;

impl App {
    pub(super) fn open_dialog(&mut self, d: Dialog, cx: &mut Cx) {
        // A question of the operation can't be covered: the worker waits for its answer.
        if self.dlg.as_ref().is_some_and(|q| q.question) && !d.question {
            self.queued.push(d);
            return;
        }
        d.write(cx.data);
        let field = match d.kind {
            Kind::Input => Some("dlg-input".to_string()),
            Kind::Props if d.name.is_some() => Some("dlg-name".to_string()),
            // The first field to type in.
            Kind::Params => d.params.iter().position(|p| matches!(p, vfs::Param::Text { .. })).map(|i| format!("dlg-p{i}")),
            _ => None,
        };
        self.dlg = Some(d);
        if let Some(f) = field {
            cx.focus(&f);
        } else {
            cx.blur();
        }
    }

    /// A passing notice (copied, done): in the corner for a few seconds, nothing to close.
    pub(super) fn toast(&mut self, text: &str, cx: &mut Cx) {
        self.toast += 1;
        let (n, mb) = (self.toast, self.mb.clone());
        cx.data.set("toast.text", text);
        cx.data.set("toast.on", true);
        spawn(move || {
            std::thread::sleep(TOAST_FOR);
            mb.send(Msg::ToastOff { n });
        });
    }

    /// A result that came by itself (an error, a note): shown now, or after the dialog that is open.
    pub(super) fn notify(&mut self, d: Dialog, cx: &mut Cx) {
        if self.dlg.is_some() {
            self.queued.push(d);
        } else {
            self.open_dialog(d, cx);
        }
    }

    pub(super) fn dialog_key(&mut self, key: &str, cx: &mut Cx) {
        let Some(d) = &mut self.dlg else { return };
        match key {
            // Escape is the button that cancels: Close or Cancel, the last one of them.
            "escape" => {
                let cancel = |a: &Act| matches!(a, Act::Close | Act::Answer(Answer::Cancel));
                let i = d.buttons.iter().rposition(|b| cancel(&b.act)).unwrap_or(d.buttons.len().saturating_sub(1));
                self.press(i, cx);
            }
            "enter" => {
                if d.kind == Kind::List {
                    let sel = d.sel;
                    self.pick(sel, cx);
                } else {
                    self.press(0, cx);
                }
            }
            // Delete on a remembered connection forgets it.
            "delete" | "backspace" if d.kind == Kind::List => self.forget_connection(cx),
            "up" | "down" if d.kind == Kind::List && d.items.iter().any(|it| !it.header) => {
                // Group titles are passed over.
                let n = d.items.len();
                loop {
                    d.moved = true;
                    d.sel = if key == "up" { (d.sel + n - 1) % n } else { (d.sel + 1) % n };
                    if !d.items[d.sel].header {
                        break;
                    }
                }
                d.write_sel(cx.data);
            }
            "home" | "end" if d.kind == Kind::List => {
                let rows = d.items.iter().enumerate().filter(|(_, it)| !it.header).map(|(i, _)| i);
                if let Some(i) = if key == "home" { rows.min() } else { rows.max() } {
                    d.moved = true;
                    d.sel = i;
                    d.write_sel(cx.data);
                }
            }
            "tab" | "shift+tab" => cx.pass_key(),
            _ => {}
        }
    }

    /// Button `i` of the dialog.
    pub(super) fn press(&mut self, i: usize, cx: &mut Cx) {
        // Connect and Test of a connection form: it stays until the connection works.
        if let Some(Act::Connect { side, index, edit, test }) = self.dlg.as_ref().and_then(|d| d.buttons.get(i)).map(|b| b.act.clone()) {
            return self.form_connect(side, index, edit, test, cx);
        }
        let Some(d) = self.dlg.take() else { return };
        // A connection form being closed: what it waits for is of no use now.
        if d.is_form() {
            self.form = None;
        }
        // The Alt+F1 list goes: the roots still being read are of no use.
        if matches!(d.buttons.first().map(|b| &b.act), Some(Act::PickRoot { .. }))
            && let Some(r) = self.roots.take()
        {
            r.cancel.cancel();
        }
        // What was typed, before `clear` wipes it (passwords must not stay in the data).
        let value = cx.data.str("dlg.value").trim().to_string();
        let args = d.args(cx.data);
        Dialog::clear(cx.data);
        cx.blur();
        let Some(act) = d.buttons.get(i).map(|b| b.act.clone()) else { return };
        let renaming = d.buttons.first().is_some_and(|b| matches!(b.act, Act::RenameTo));
        match act {
            // Cancel in "Rename…": back to the question it came from.
            Act::Close if renaming => {
                if let Some(q) = self.question.clone() {
                    self.open_dialog(q, cx);
                }
            }
            // Closing a question without an answer would leave the operation waiting.
            Act::Close if d.question => self.answer(Answer::Cancel, cx),
            Act::Close => {
                if let Some(next) = (!self.queued.is_empty()).then(|| self.queued.remove(0)) {
                    self.open_dialog(next, cx);
                }
            }
            Act::ConfirmConnect => {
                if let Some((side, index, args, dir)) = self.confirming.take() {
                    self.connect(side, index, args, dir, false, cx);
                }
            }
            Act::QuitNow => {
                self.quitting = true;
                if let Some(j) = &self.job {
                    j.cancel.cancel();
                    // Answer a question the worker may be waiting on, so it can see the cancel.
                    let _ = j.answer.send(Answer::Cancel);
                }
                // The worker cleans up (temporary files, unfinished uploads) and says Done; not forever.
                let mb = self.mb.clone();
                spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    mb.send(Msg::Quit);
                });
            }
            Act::Transfer { mode, side, names, here } => {
                if !value.is_empty() {
                    self.start_transfer(mode, side, names, value, here, cx);
                }
            }
            Act::NewFile { side } => {
                let p = &self.panels[side];
                let (src, dir, mb) = (p.src.clone(), p.dir.clone(), self.mb.clone());
                spawn(move || {
                    let res = ops::new_file(&*src, &dir, &value);
                    mb.send(Msg::NewFile { side, res });
                });
            }
            // Handled above: the dialog stays.
            Act::Connect { .. } => {}
            Act::Mkdir { side } => {
                if value.is_empty() {
                    return;
                }
                let p = &self.panels[side];
                let (src, dir, mb) = (p.src.clone(), p.dir.clone(), self.mb.clone());
                spawn(move || {
                    let res = ops::mkdir(&*src, &dir, &value);
                    mb.send(Msg::Mkdir { side, res });
                });
            }
            Act::Mask { side, select } => {
                let p = &mut self.panels[side];
                for it in &mut p.items {
                    // A mask selects files; `*` alone takes folders too.
                    if ops::matches_mask(&it.entry.name, &value) && (!it.entry.is_dir() || value.split([';', ' ']).any(|m| m == "*")) {
                        it.sel = select;
                    }
                }
                self.write_panel(side, cx.data);
            }
            Act::Delete { side, names } => {
                let p = &self.panels[side];
                let (src, dir) = (p.src.clone(), p.dir.clone());
                let what = if names.len() == 1 { names[0].clone() } else { format::count(names.len() as u64, "object") };
                let title = t!("Deleting {}", what);
                self.start_job(title, Some(side), None, cx, move |fb, c| ops::delete(&*src, &dir, &names, fb, c));
            }
            Act::Answer(a) => self.answer(a, cx),
            Act::AskRename => {
                let mut d = Dialog::input(t!("Rename"), t!("New name:"), "", t!("OK"), Act::RenameTo);
                d.question = true;
                self.open_dialog(d, cx);
            }
            Act::RenameTo => {
                if vfs::valid_name(&value) {
                    self.answer(Answer::Rename(value), cx);
                } else if let Some(q) = self.question.clone() {
                    // Not a name ("", "a/b", ".."): the question again.
                    self.open_dialog(q, cx);
                }
            }
            Act::PickRoot { side } => {
                let _ = side;
                self.pick_from(&d, d.sel, cx);
            }
            Act::RunAction { side, action, targets } => {
                self.run_action(side, &action.id, targets, args);
            }
            Act::ActionParams { side, index } => self.source_action(side, index, cx),
            Act::ViewSearch => {
                if !value.is_empty() {
                    self.view_search(value, false);
                }
            }
            Act::ApplyProps { side, name, mode } => self.apply_props(side, &d, name, mode, cx),
        }
    }

    /// A row of the list dialog: a root or a source.
    pub(super) fn pick(&mut self, i: usize, cx: &mut Cx) {
        let Some(d) = self.dlg.take() else { return };
        Dialog::clear(cx.data);
        self.pick_from(&d, i, cx);
    }

    /// Row `i` of the list dialog `d`, already taken off the screen (the Open button comes this way).
    pub(super) fn pick_from(&mut self, d: &Dialog, i: usize, cx: &mut Cx) {
        if let Some(r) = self.roots.take() {
            r.cancel.cancel();
        }
        let Act::PickRoot { side } = d.buttons.first().map(|b| b.act.clone()).unwrap_or(Act::Close) else { return };
        let Some(it) = d.items.get(i).filter(|it| !it.header) else { return };
        self.set_active(side, cx);
        if let Some(pi) = it.profile {
            // Connected already: just go there.
            let on = self.profiles.get(pi).and_then(|p| self.sources.iter().find(|s| s.info().id == p.id)).cloned();
            if let Some(src) = on {
                let home = src.home();
                return self.load_from(side, src, home, None);
            }
            return self.connect_saved(side, pi, None, false, cx);
        }
        let src = self.sources[it.source].clone();
        self.load_from(side, src, it.path.clone(), None);
    }

    /// Alt+F1 / Alt+F2: the list opens at once; each source's roots are read in a worker of their own and
    /// come in as they are ready, so a slow or dead server holds up only its own line.
    pub(super) fn pick_source(&mut self, side: usize, cx: &mut Cx) {
        if let Some(r) = self.roots.take() {
            r.cancel.cancel();
        }
        let seq = self.next_seq();
        let cancel = Cancel::new();
        for (index, src) in self.sources.iter().enumerate() {
            let (src, mb, cancel) = (src.clone(), self.mb.clone(), cancel.clone());
            spawn(move || {
                let res = src.roots(&cancel);
                if res != Err(vfs::Error::Cancelled) {
                    mb.send(Msg::Roots { seq, index, res });
                }
            });
        }
        self.roots = Some(RootsList { seq, side, results: vec![None; self.sources.len()], cancel });
        self.show_roots(cx, true);
    }

    /// The Alt+F1 list as far as it is read (`open` — a new dialog; otherwise the open one is refreshed).
    pub(super) fn show_roots(&mut self, cx: &mut Cx, open: bool) {
        let Some(RootsList { side, results, .. }) = &self.roots else { return };
        let side = *side;
        let mut items = Vec::new();
        let cur = self.source_index(&self.panels[side].src);
        // The panel's own root: the longest that holds its folder (`~` rather than `/`).
        let (mut here, mut best) = (0, 0);
        // A group per source, under its name; the rows say only what differs (the root and its path).
        for (i, r) in results.iter().enumerate() {
            let title = self.sources[i].info().title;
            let item = |label: String, note: String, path: VPath| ListItem { label, note, source: i, path, profile: None, header: false };
            items.push(ListItem { header: true, ..item(source_title(&title), String::new(), VPath::root()) });
            match r {
                None => items.push(item(t!("reading…").into(), String::new(), VPath::root())),
                Some(Ok(list)) => {
                    for root in list {
                        let path = root.path.to_string();
                        if i == cur && self.panels[side].dir.starts_with(&root.path) && path.len() > best {
                            (here, best) = (items.len(), path.len());
                        }
                        // The path only where it says more than the name (`~` — /Users/x; not `/` — /).
                        let note = if path == root.title { String::new() } else { path };
                        items.push(item(crate::i18n::tr_quiet(&root.title).to_string(), note, root.path.clone()));
                    }
                }
                Some(Err(e)) => items.push(item(crate::i18n::err(e), String::new(), VPath::root())),
            }
        }
        // Remembered connections that are not connected now.
        let saved: Vec<(usize, &crate::connect::Profile)> =
            self.profiles.iter().enumerate().filter(|(_, p)| !self.sources.iter().any(|s| s.info().id == p.id)).collect();
        if !saved.is_empty() {
            items.push(ListItem {
                label: t!("Saved connections").into(),
                note: String::new(),
                source: 0,
                path: VPath::root(),
                profile: None,
                header: true,
            });
        }
        for (pi, p) in saved {
            let note = if p.secrets.is_empty() { t!("Enter connects · Delete forgets") } else { t!("asks the password · Delete forgets") };
            items.push(ListItem { label: p.title.clone(), note: note.into(), source: 0, path: VPath::root(), profile: Some(pi), header: false });
        }
        // The first row that can be chosen, if the panel's folder is under none.
        if items.get(here).is_none_or(|it| it.header) {
            here = items.iter().position(|it| !it.header).unwrap_or(0);
        }
        if open {
            let title = if side == 0 { t!("Source and folder for the left panel") } else { t!("Source and folder for the right panel") };
            let mut d =
                Dialog::new(Kind::List, title, "").buttons(vec![Button::new(t!("Open"), "primary", Act::PickRoot { side }), Button::cancel()]);
            d.drop = Some(side);
            d.items = items;
            d.sel = here;
            self.open_dialog(d, cx);
        } else if let Some(d) = self.dlg.as_mut().filter(|d| matches!(d.buttons.first().map(|b| &b.act), Some(Act::PickRoot { .. }))) {
            // The row the user moved to stays chosen, wherever the rows that came put it; until then the
            // choice follows the panel's folder.
            let same =
                |a: &ListItem, b: &ListItem| (a.source, &a.path, &a.label, a.profile, a.header) == (b.source, &b.path, &b.label, b.profile, b.header);
            let kept = d.moved.then(|| d.items.get(d.sel)).flatten().and_then(|was| items.iter().position(|it| same(it, was)));
            let sel = kept.unwrap_or(here);
            d.items = items;
            d.sel = sel;
            d.write(cx.data);
        }
    }
}
