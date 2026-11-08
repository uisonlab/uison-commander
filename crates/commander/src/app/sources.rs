//! Properties, a source's own actions, and connecting to storages (remembered ones, the Keychain, trust).

use super::*;

impl App {
    pub(super) fn properties(&mut self, cx: &mut Cx) {
        let a = self.active;
        let p = &self.panels[a];
        let Some(e) = p.current().cloned() else { return };
        let path = p.dir.child(&e.name);
        let kind = match &e.kind {
            EntryKind::File => match vfs::split_ext(&e.name).1 {
                "" => t!("File").to_string(),
                x => t!("{} file", x.to_uppercase()),
            },
            EntryKind::Dir => t!("Folder").to_string(),
            EntryKind::Symlink { target, to_dir } => {
                let to = target.clone().unwrap_or_default();
                if *to_dir { t!("Link to a folder → {}", to) } else { t!("Link to a file → {}", to) }
            }
            EntryKind::Other => t!("Not a regular file (a pipe, socket or device)").to_string(),
        };
        let caps = p.src.info().caps;
        // A link's permissions are its target's: chmod follows the link.
        let link = matches!(e.kind, EntryKind::Symlink { .. });
        let rename = caps.write && caps.rename;
        let chmod = caps.write && caps.permissions && !link && e.mode.is_some();
        let mut props = Vec::new();
        if !rename {
            props.push((t!("Name").to_string(), e.name.clone()));
        }
        props.extend([
            (t!("Path").to_string(), path.to_string()),
            (t!("Source").into(), source_title(&p.src.info().title)),
            (t!("Kind").into(), kind),
        ]);
        if let Some(s) = e.size.or_else(|| p.sizes.get(&e.name).copied()) {
            props.push((t!("Size").into(), format!("{} ({})", format::count(s, "byte"), format::short(s))));
        }
        if let Some(t) = e.modified {
            props.push((t!("Modified").into(), format::date_long(t)));
        }
        if let Some(m) = e.mode.filter(|_| !chmod) {
            props.push((t!("Permissions").into(), format!("{} ({:o})", format::mode(m), m)));
        }
        // The source's own lines (S3: storage class), their names in the current language.
        props.extend(e.extra.iter().map(|(k, v)| (t!(k).to_string(), v.clone())));
        let actions = p.src.actions(&p.dir, std::slice::from_ref(&e));
        // OK (or Close, if nothing can be changed) is the default; the source's actions follow.
        let mut buttons = if rename || chmod {
            let act = Act::ApplyProps { side: a, name: e.name.clone(), mode: e.mode.filter(|_| chmod) };
            vec![Button::new(t!("OK"), "primary", act), Button::cancel()]
        } else {
            vec![Button::new(t!("Close"), "primary", Act::Close)]
        };
        for (i, a) in actions.iter().enumerate().take(ACTION_SLOTS) {
            let title = if a.params.is_empty() { t!(&a.title).to_string() } else { format!("{}…", t!(&a.title)) };
            buttons.push(Button::new(&title, "secondary", Act::ActionParams { side: self.active, index: i }));
        }
        let mut d = Dialog::new(Kind::Props, t!("Properties"), "").buttons(buttons);
        d.props = props;
        d.name = rename.then(|| e.name.clone());
        d.mode = e.mode.filter(|_| chmod);
        self.open_dialog(d, cx);
    }

    /// Connect in a worker with these answers; "Connecting…" meanwhile (unless `quiet`: at startup).
    pub(super) fn connect(&mut self, side: usize, index: usize, args: vfs::ActionArgs, dir: Option<VPath>, quiet: bool, cx: &mut Cx) {
        if !quiet && self.connecting.is_some() {
            return self.notify(Dialog::message(t!("Connecting"), t!("Another connection is being made.")), cx);
        }
        let Some(c) = crate::connect::all().into_iter().nth(index) else { return };
        let seq = self.next_seq();
        if !quiet {
            self.connecting = Some(seq);
            let to = args.0.first().map(|(_, v)| v.trim().to_string()).filter(|v| !v.is_empty()).unwrap_or_else(|| c.title.into());
            cx.data.set("conn.text", t!("Connecting to {}…", to));
            cx.data.set("conn.on", true);
        }
        self.spawn_connect(side, seq, index, args, dir, quiet, None, false);
    }

    /// The worker of a connection. Checks and screenshots (`!persist`) keep nothing in the Keychain.
    /// `fill` — a remembered connection being changed: its secrets left empty come from the Keychain.
    /// `test` — only see that it connects: nothing is kept.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_connect(
        &self,
        side: usize,
        seq: u64,
        index: usize,
        mut args: vfs::ActionArgs,
        dir: Option<VPath>,
        quiet: bool,
        fill: Option<crate::connect::Profile>,
        test: bool,
    ) {
        let Some(c) = crate::connect::all().into_iter().nth(index) else { return };
        let (mb, persist) = (self.mb.clone(), self.persist);
        spawn(move || {
            if let Some(old) = fill.filter(|_| persist) {
                for (k, v) in &mut args.0 {
                    if v.is_empty()
                        && old.secrets.contains(k)
                        && let Some(kept) = crate::secrets::load(&old.id, k)
                    {
                        *v = kept;
                    }
                }
            }
            Self::connect_worker(&c, side, seq, index, args, dir, quiet, persist && !test, &mb)
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn connect_worker(
        c: &crate::connect::Connector,
        side: usize,
        seq: u64,
        index: usize,
        args: vfs::ActionArgs,
        dir: Option<VPath>,
        quiet: bool,
        keep: bool,
        mb: &Mailbox,
    ) {
        match (c.connect)(&args) {
            Ok((s, note)) => {
                let p = crate::connect::Profile::of(c, &args, &*s);
                // The secrets typed: into the Keychain if asked to remember them, out of it if not.
                if keep && args.get("remember").is_some() {
                    let remember = args.get("remember") == Some("true");
                    for k in &p.secrets {
                        match args.get(k).filter(|_| remember) {
                            Some(v) => {
                                crate::secrets::store(&p.id, k, v);
                            }
                            None => crate::secrets::forget(&p.id, k),
                        }
                    }
                }
                mb.send(Msg::Connected { side, seq, res: Ok(s), note, profile: Some(p), dir, quiet });
            }
            Err(crate::connect::Failure::Error(e)) => mb.send(Msg::Connected { side, seq, res: Err(e), note: None, profile: None, dir, quiet }),
            Err(crate::connect::Failure::Confirm { title, text, button, key, value }) => {
                mb.send(Msg::Confirm { side, seq, index, args, title, text, button, key, value, dir, quiet })
            }
        }
    }

    /// Stop waiting for the connection being made: its answer, when it comes, is ignored.
    pub(super) fn cancel_connect(&mut self, cx: &mut Cx) {
        self.connecting = None;
        cx.data.set("conn.on", false);
    }

    /// A remembered connection: at once if it needs no secret or the Keychain has them (read in a worker:
    /// the system may ask the user first), otherwise its dialog with the rest filled in.
    pub(super) fn connect_saved(&mut self, side: usize, i: usize, dir: Option<VPath>, quiet: bool, cx: &mut Cx) {
        let Some(p) = self.profiles.get(i).cloned() else { return };
        let Some(index) = crate::connect::all().iter().position(|c| c.id == p.kind) else { return };
        if p.secrets.is_empty() {
            return self.connect(side, index, vfs::ActionArgs(p.args.clone()), dir, quiet, cx);
        }
        let mb = self.mb.clone();
        spawn(move || {
            let mut args = p.args.clone();
            let mut all = crate::secrets::available();
            for k in &p.secrets {
                match crate::secrets::load(&p.id, k) {
                    Some(v) => args.push((k.clone(), v)),
                    None => all = false,
                }
            }
            mb.send(Msg::Secrets { side, profile: i, args: all.then_some(vfs::ActionArgs(args)), dir, quiet });
        });
    }

    /// The form of a remembered connection: its secrets are not at hand (`edit` false), or it is changed.
    pub(super) fn ask_saved(&mut self, side: usize, i: usize, edit: bool, cx: &mut Cx) {
        let Some(p) = self.profiles.get(i).cloned() else { return };
        let all = crate::connect::all();
        let Some(index) = all.iter().position(|c| c.id == p.kind) else { return };
        let c = &all[index];
        let text = if edit { t!("{} — change and connect.", p.title) } else { t!("{} — the password or key is not remembered.", p.title) };
        // Changing it, an empty secret means the one remembered.
        let kept: Vec<String> = if edit && crate::secrets::available() { p.secrets.clone() } else { Vec::new() };
        self.connect_form(side, index, p.params(c), &t!("{} connection", crate::i18n::tr_quiet(c.label)), &text, Some(i), &kept, cx);
    }

    /// "New S3…", "New SFTP…" (`server` — a Host of ~/.ssh/config to start from).
    pub(super) fn ask_connect(&mut self, side: usize, index: usize, server: Option<&str>, cx: &mut Cx) {
        let Some(c) = crate::connect::all().into_iter().nth(index) else { return };
        let mut params = (c.params)();
        if let (Some(server), Some(vfs::Param::Text { default, .. })) = (server, params.first_mut()) {
            *default = server.to_string();
        }
        self.connect_form(side, index, params, &t!("New {} connection", crate::i18n::tr_quiet(c.label)), t!(c.intro), None, &[], cx);
    }

    /// A connection form: Connect (stays open until it works, and says why not), Test, Cancel.
    /// `kept` — secrets the Keychain has: their fields may stay empty.
    #[allow(clippy::too_many_arguments)]
    fn connect_form(
        &mut self,
        side: usize,
        index: usize,
        params: Vec<vfs::Param>,
        title: &str,
        text: &str,
        edit: Option<usize>,
        kept: &[String],
        cx: &mut Cx,
    ) {
        let Some(c) = crate::connect::all().into_iter().nth(index) else { return };
        let mut d = Dialog::new(Kind::Params, title, text);
        d.params = with_remember(params);
        d.hints = c.hints.iter().map(|(k, h)| (k.to_string(), h.to_string())).collect();
        for k in kept {
            d.hints.retain(|(id, _)| id != k);
            d.hints.push((k.clone(), "empty — the one in the Keychain".into()));
        }
        d.files = c.files.iter().map(|f| f.to_string()).collect();
        d.buttons = vec![
            Button::new(t!("Connect"), "primary", Act::Connect { side, index, edit, test: false }),
            Button::new(t!("Test"), "secondary", Act::Connect { side, index, edit, test: true }).left(),
            Button::cancel(),
        ];
        self.form = Some(Form::default());
        self.open_dialog(d, cx);
    }

    /// Connect or Test in the form: the form stays and says what happens.
    pub(super) fn form_connect(&mut self, side: usize, index: usize, edit: Option<usize>, test: bool, cx: &mut Cx) {
        let seq = self.next_seq();
        let Some(d) = &mut self.dlg else { return };
        let mut args = d.args(cx.data);
        let form = self.form.get_or_insert_with(Form::default);
        if let Some((k, v)) = &form.trust {
            args.0.retain(|(x, _)| x != k);
            args.0.push((k.clone(), v.clone()));
        }
        form.seq = Some(seq);
        form.test = test;
        form.edit = edit;
        d.status = Some((Status::Busy, if test { t!("Checking the connection…") } else { t!("Connecting…") }.into()));
        d.write_status(cx.data);
        let fill = edit.and_then(|i| self.profiles.get(i).cloned());
        self.spawn_connect(side, seq, index, args, None, false, fill, test);
    }

    /// The connection form: open (`true`), or waiting behind a question of an operation.
    pub(super) fn form_dialog(&mut self) -> Option<(&mut Dialog, bool)> {
        if self.dlg.as_ref().is_some_and(Dialog::is_form) {
            return self.dlg.as_mut().map(|d| (d, true));
        }
        self.queued.iter_mut().find(|d| d.is_form()).map(|d| (d, false))
    }

    /// The form's line: why it failed, that it works, a question.
    pub(super) fn form_status(&mut self, status: Status, text: String, cx: &mut Cx) {
        if let Some(f) = &mut self.form {
            f.seq = None;
        }
        if let Some((d, open)) = self.form_dialog() {
            d.status = Some((status, text));
            if open {
                d.write_status(cx.data);
            }
        }
    }

    /// The connection worked: the form goes (and the secrets typed with it), then whatever waited.
    pub(super) fn close_form(&mut self, cx: &mut Cx) {
        self.form = None;
        if self.dlg.as_ref().is_some_and(Dialog::is_form) {
            self.dlg = None;
            Dialog::clear(cx.data);
            cx.blur();
            if !self.queued.is_empty() {
                let next = self.queued.remove(0);
                self.open_dialog(next, cx);
            }
        } else {
            self.queued.retain(|d| !d.is_form());
        }
    }

    /// The Connect menu (`conn.menu`: a new connection of each kind, then Edit… — the remembered ones) and
    /// the connections' Add menu (`conn.addMenu`: the kinds only). Written again in another language.
    pub(super) fn write_connect_menus(d: &mut Data) {
        let all = crate::connect::all();
        let kinds: Vec<Json> = crate::connect::menu_order()
            .into_iter()
            .map(|i| obj([("label", Json::from(crate::i18n::tr_quiet(all[i].label))), ("action", Json::from(format!("connNew{i}")))]))
            .collect();
        d.set("conn.addMenu", Json::Array(kinds.clone()));
        let mut menu = kinds;
        menu.push(Json::from("-"));
        menu.push(obj([("label", Json::from(t!("Edit…"))), ("action", Json::from("connList"))]));
        d.set("conn.menu", Json::Array(menu));
    }

    /// Edit… of the Connect menu: the connections the user added (not the hosts of ~/.ssh/config), with Add,
    /// Edit, Delete and Close beside them. Enter or a double click connects the chosen one.
    pub(super) fn connections(&mut self, cx: &mut Cx) {
        let side = self.active;
        let items: Vec<ListItem> = self
            .profiles
            .iter()
            .enumerate()
            .map(|(pi, p)| {
                let note = if self.sources.iter().any(|s| s.info().id == p.id) {
                    t!("connected")
                } else if p.secrets.is_empty() {
                    ""
                } else {
                    t!("asks the password")
                };
                ListItem { label: p.title.clone(), note: note.into(), source: 0, path: VPath::root(), profile: Some(pi), header: false }
            })
            .collect();
        let text = if items.is_empty() {
            t!("Nothing here yet: add a connection — it is kept here once it works.")
        } else {
            t!("Enter or a double click connects. Passwords are kept in the Keychain.")
        };
        let buttons = vec![Button::new(t!("Connect"), "primary", Act::PickRoot { side }), Button::new(t!("Close"), "secondary", Act::Close)];
        let mut d = Dialog::new(Kind::List, t!("Connections"), text).buttons(buttons);
        d.items = items;
        d.side = true;
        self.open_dialog(d, cx);
    }

    /// Edit… in the connections dialog: the chosen one in its form.
    pub(super) fn edit_connection(&mut self, cx: &mut Cx) {
        let Some(pi) = self.dlg.as_ref().and_then(|d| d.items.get(d.sel)).and_then(|it| it.profile) else { return };
        self.ask_saved(self.active, pi, true, cx);
    }

    /// Delete in the source list or the connections dialog: forget the remembered connection chosen.
    pub(super) fn forget_connection(&mut self, cx: &mut Cx) {
        let Some(d) = &mut self.dlg else { return };
        let Some(pi) = d.items.get(d.sel).and_then(|it| it.profile) else { return };
        let p = self.profiles.remove(pi);
        if self.persist {
            for k in &p.secrets {
                crate::secrets::forget(&p.id, k);
            }
            crate::connect::save(&self.profiles);
        }
        d.items.remove(d.sel);
        for it in &mut d.items {
            it.profile = it.profile.map(|x| if x > pi { x - 1 } else { x });
        }
        // A group title with nothing under it goes too.
        let lone = |items: &[ListItem], i: usize| items[i].header && items.get(i + 1).is_none_or(|n| n.header);
        while let Some(i) = (0..d.items.len()).find(|&i| lone(&d.items, i)) {
            d.items.remove(i);
        }
        d.sel = d.sel.min(d.items.len().saturating_sub(1));
        if d.items.get(d.sel).is_some_and(|it| it.header) {
            d.sel = d.items.iter().position(|it| !it.header).unwrap_or(0);
        }
        if d.side && d.items.is_empty() {
            d.text = t!("Nothing here yet: add a connection — it is kept here once it works.").to_string();
        }
        d.write(cx.data);
    }

    /// Rename and chmod as the properties dialog says; only what changed.
    pub(super) fn apply_props(&mut self, side: usize, d: &Dialog, name: String, mode: Option<u32>, cx: &mut Cx) {
        let new_name = cx.data.str("dlg.name").trim().to_string();
        let rename = d.name.is_some() && new_name != name;
        if rename && (new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains('/')) {
            self.open_dialog(Dialog::message(t!("Cannot rename"), &t!("\"{}\" is not a valid name", new_name)), cx);
            return;
        }
        let chmod = d.ticked_mode(cx.data).filter(|m| Some(*m) != mode);
        if !rename && chmod.is_none() {
            return;
        }
        let p = &self.panels[side];
        let (src, dir, mb) = (p.src.clone(), p.dir.clone(), self.mb.clone());
        spawn(move || {
            let path = dir.child(&name);
            let res = (|| {
                if let Some(m) = chmod {
                    src.set_mode(&path, m)?;
                }
                if rename {
                    src.rename(&path, &dir.child(&new_name))?;
                    return Ok(new_name);
                }
                Ok(name)
            })();
            mb.send(Msg::Changed { side, res });
        });
    }

    /// Source action `index` for the panel's targets: parameters first, if it has any.
    pub(super) fn source_action(&mut self, side: usize, index: usize, _cx: &mut Cx) {
        let p = &self.panels[side];
        let targets = p.targets();
        let Some(action) = p.src.actions(&p.dir, &targets).into_iter().nth(index) else { return };
        let paths: Vec<VPath> = targets.iter().map(|e| p.dir.child(&e.name)).collect();
        if action.params.is_empty() {
            self.run_action(side, &action.id, paths, vfs::ActionArgs::default());
            return;
        }
        // The defaults are what is there now (S3: the current visibility): the source is asked in a worker first.
        let (src, mb) = (p.src.clone(), self.mb.clone());
        spawn(move || {
            let res = src.prepare_action(&action, &paths, &Cancel::new());
            mb.send(Msg::ActionReady { side, targets: paths, res });
        });
    }

    /// The parameters of a source action, filled in by the source.
    pub(super) fn action_dialog(&mut self, side: usize, action: vfs::ActionInfo, targets: Vec<VPath>, cx: &mut Cx) {
        let what = if targets.len() == 1 { targets[0].name().to_string() } else { format::count(targets.len() as u64, "object") };
        let mut d = Dialog::new(Kind::Params, t!(&action.title), &what);
        d.params = action.params.clone();
        d.buttons = vec![Button::new(t!("OK"), "primary", Act::RunAction { side, action, targets }), Button::cancel()];
        self.open_dialog(d, cx);
    }

    pub(super) fn run_action(&mut self, side: usize, id: &str, targets: Vec<VPath>, args: vfs::ActionArgs) {
        let (src, mb, id) = (self.panels[side].src.clone(), self.mb.clone(), id.to_string());
        spawn(move || {
            let res = src.run_action(&id, &targets, &args, &Cancel::new());
            mb.send(Msg::Action { res });
        });
    }
}
