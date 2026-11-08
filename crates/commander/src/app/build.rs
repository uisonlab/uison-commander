//! The screen with all its handlers, and where the panels start.

use super::*;

/// Where the panels start.
pub enum Start {
    /// As they were last time; a folder from the command line goes to the left panel.
    Saved(Option<VPath>),
    /// Exactly these (source index, folder), and nothing is saved: checks and screenshots.
    Fixed([(usize, VPath); 2]),
}

/// The screen with all handlers.
pub fn build(sources: Vec<Arc<dyn Source>>, start: Start) -> Ui {
    let st = match &start {
        Start::Saved(_) => state::load(),
        Start::Fixed(_) => None,
    };
    // The language before the screen: its texts are built in it (checks keep the one they set).
    if let Some(s) = &st {
        crate::i18n::set(s.lang);
    }
    let theme = st.as_ref().map_or_else(|| "system".to_string(), |s| s.theme.clone());
    let mut ui = Ui::from_node(crate::screen::screen()).with_size(1200.0, 760.0);
    ui.set_theme(&theme);
    let mb = Mailbox::new(ui.handle());
    let mk = |side: &'static str, i: usize| -> Panel {
        let saved = st.as_ref().map(|s| &s.panels[i]);
        let found = saved.and_then(|p| sources.iter().find(|s| s.info().id == p.src)).cloned();
        // A source of the last session that is not here now (S3 is per session): start at home.
        let dir = saved.filter(|_| found.is_some()).map(|p| p.dir.clone());
        let src = found.unwrap_or_else(|| sources[0].clone());
        let dir = dir.unwrap_or_else(|| src.home());
        let mut p = Panel::new(side, src, dir);
        if let Some(s) = saved {
            p.sort = s.sort;
            p.show_hidden = s.hidden;
        }
        p
    };
    let mut panels = [mk("left", 0), mk("right", 1)];
    let profiles = match &start {
        Start::Saved(_) => crate::connect::load(),
        Start::Fixed(_) => Vec::new(),
    };
    // A panel that was on a remembered connection: it connects again by itself (if no secret is needed)
    // and opens where it was; meanwhile it shows the local home.
    let restore: Vec<(usize, usize, VPath)> = (0..2)
        .filter_map(|i| {
            let saved = &st.as_ref()?.panels[i];
            if sources.iter().any(|s| s.info().id == saved.src) {
                return None;
            }
            let pi = profiles.iter().position(|p| p.id == saved.src)?;
            Some((i, pi, saved.dir.clone()))
        })
        .collect();
    match &start {
        Start::Saved(Some(dir)) => {
            panels[0].src = sources[0].clone();
            panels[0].dir = dir.clone();
        }
        Start::Saved(None) => {}
        Start::Fixed(f) => {
            for (p, (i, dir)) in panels.iter_mut().zip(f) {
                p.src = sources[*i].clone();
                p.dir = dir.clone();
            }
        }
    }
    let persist = matches!(start, Start::Saved(_));
    let active = st.as_ref().map_or(0, |s| s.active);
    let app = Rc::new(RefCell::new(App {
        panels,
        active,
        sources,
        mb,
        dlg: None,
        job: None,
        view: None,
        queued: Vec::new(),
        question: None,
        quitting: false,
        toast: 0,
        cols: st.as_ref().map_or([None, None], |s| [s.panels[0].cols, s.panels[1].cols]),
        profiles,
        connecting: None,
        confirming: None,
        form: None,
        roots: None,
        theme,
        settings: false,
        seq: 0,
        saved: String::new(),
        persist,
    }));
    {
        let mut a = app.borrow_mut();
        a.write_panels(&mut ui.data);
        for side in 0..2 {
            let dir = a.panels[side].dir.clone();
            a.load(side, dir, None);
        }
    }
    {
        // Each panel's connection: its secrets (if any) from the Keychain in a worker, then the quiet path of
        // `connect` (no "Connecting…", errors as a note).
        let a = app.borrow();
        for (side, pi, dir) in restore {
            let Some(p) = a.profiles.get(pi).cloned() else { continue };
            let mb = a.mb.clone();
            spawn(move || {
                let mut args = p.args.clone();
                let mut all = true;
                for k in &p.secrets {
                    match crate::secrets::load(&p.id, k) {
                        Some(v) => args.push((k.clone(), v)),
                        None => all = false,
                    }
                }
                mb.send(Msg::Secrets { side, profile: pi, args: all.then_some(vfs::ActionArgs(args)), dir: Some(dir), quiet: true });
            });
        }
    }
    {
        let a = app.borrow();
        a.write_settings(&mut ui.data);
        App::write_connect_menus(&mut ui.data);
        a.write_dark(&mut ui.data);
        // The window's frame in the chosen look from the start.
        #[cfg(target_os = "macos")]
        if persist && a.theme != "system" {
            crate::macos::set_appearance(&a.theme);
        }
    }
    ui.data.set("dlg.open", false);
    ui.data.set("dlg.about", false);
    ui.data.set("drop.side", "");
    ui.data.set("conn.on", false);
    for (s, cols) in ["left", "right"].into_iter().zip(app.borrow().cols) {
        for (k, w) in COL_KEYS.iter().zip(cols.unwrap_or(COLS)) {
            ui.data.set(&format!("colw.{s}.{k}"), w);
        }
    }
    ui.data.set("toast.on", false);
    ui.data.set("view.open", false);
    ui.data.set("job.show", false);
    ui.data.set("job.strip", false);

    on(&mut ui, &app, "key", App::key);
    on(&mut ui, &app, "pump", App::pump);
    on(&mut ui, &app, "row", App::row_click);
    // A panel's title: its drop-down of sources and drives; again — closes it.
    on(&mut ui, &app, "header", |a, cx| {
        let side = usize::from(cx.key.starts_with("right"));
        let open = a.dlg.as_ref().and_then(|d| d.drop);
        if open.is_some() {
            a.dialog_key("escape", cx);
        }
        if open != Some(side) {
            a.pick_source(side, cx);
        }
    });
    on(&mut ui, &app, "dropPick", |a, cx| {
        let Some(i) = cx.key.split("-dd").nth(1).and_then(|n| n.parse::<usize>().ok()) else { return };
        if a.dlg.as_ref().is_some_and(|d| d.drop.is_some() && d.items.get(i).is_some_and(|it| !it.header)) {
            a.pick(i, cx);
        }
    });
    // A click beside the drop-down closes it; on it (not on a row) — nothing.
    on(&mut ui, &app, "dropClose", |a, cx| {
        if a.dlg.as_ref().is_some_and(|d| d.drop.is_some()) {
            a.dialog_key("escape", cx);
        }
    });
    on(&mut ui, &app, "dropKeep", |_, _| {});
    on(&mut ui, &app, "sortcol", |a, cx| {
        let side = usize::from(cx.key.starts_with("right"));
        if let Some(k) = cx.key.rsplit('-').next().and_then(SortKey::parse) {
            a.set_active(side, cx);
            a.sort(side, k, cx);
        }
    });
    on(&mut ui, &app, "fkey", |a, cx| {
        let key = cx.key.trim_start_matches("fk-").to_string();
        if let Some(c) = command(&key) {
            a.run(c, cx);
        }
    });
    on(&mut ui, &app, "tool", |a, cx| {
        let tool = cx.key.trim_start_matches("tb-").to_string();
        match tool.as_str() {
            "refresh" => a.run(Cmd::Refresh, cx),
            "back" => a.run(Cmd::Back, cx),
            "forward" => a.run(Cmd::Forward, cx),
            "connect" => a.run(Cmd::Connect, cx),
            "cancel-connect" => a.cancel_connect(cx),
            "settings" => a.toggle_settings(cx),
            // A menu under the button: the keys, About.
            "help" => {
                a.close_settings(cx);
                cx.context_menu("tb-help");
            }
            _ => {}
        }
    });
    // The Connect menu: a new connection of a kind (its form), or the remembered ones.
    for i in 0..crate::connect::all().len() {
        on(&mut ui, &app, &format!("connNew{i}"), move |a, cx| {
            let side = a.active;
            a.ask_connect(side, i, None, cx);
        });
    }
    on(&mut ui, &app, "connList", App::connections);
    // The connections' own buttons.
    on(&mut ui, &app, "connAdd", |_, cx| cx.context_menu("conn-add"));
    on(&mut ui, &app, "connEdit", App::edit_connection);
    on(&mut ui, &app, "connDelete", App::forget_connection);
    on(&mut ui, &app, "connClose", |a, cx| a.dialog_key("escape", cx));
    on(&mut ui, &app, "setPick", App::settings_pick);
    on(&mut ui, &app, "setClose", App::close_settings);
    // The system turned light or dark: the symbols follow (with the theme "system").
    on(&mut ui, &app, "appearance", |a, cx| a.write_dark(cx.data));
    on(&mut ui, &app, "menuKeys", App::help);
    on(&mut ui, &app, "menuAbout", App::about);
    // "Choose…" by a file field (an SSH key): the system's Open dialog, in the field's folder or ~/.ssh.
    on(&mut ui, &app, "dlgBrowse", |_, cx| {
        let Some(i) = cx.key.strip_prefix("dlg-br").and_then(|n| n.parse::<usize>().ok()) else { return };
        let field = format!("dlg.params.{i}.value");
        let home = std::env::home_dir().unwrap_or_default();
        let typed = cx.data.str(&field).trim().to_string();
        let typed = match typed.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => std::path::PathBuf::from(typed),
        };
        let start = typed.parent().filter(|p| p.is_absolute()).map_or_else(|| home.join(".ssh"), |p| p.to_path_buf());
        #[cfg(target_os = "macos")]
        let chosen = crate::macos::choose_file(&start, t!("Choose the private key (not the .pub file)"));
        #[cfg(windows)]
        let chosen = crate::windows::choose_file(&start, t!("Choose the private key (not the .pub file)"));
        #[cfg(not(any(target_os = "macos", windows)))]
        let chosen: Option<String> = {
            let _ = start;
            None
        };
        if let Some(path) = chosen {
            // Under the home folder as ~/…: shorter, and the same on another computer (`/` on Windows too).
            let shown =
                std::path::Path::new(&path).strip_prefix(&home).map_or(path.clone(), |r| format!("~/{}", r.to_string_lossy().replace('\\', "/")));
            cx.data.set(&field, shown);
        }
    });
    // Properties: the number follows the ticks.
    on(&mut ui, &app, "permTick", |_, cx| {
        let m = crate::dialog::ticks(cx.data);
        cx.data.set("dlg.octal", crate::dialog::octal(m));
    });
    on(&mut ui, &app, "dlgOk", |a, cx| a.press(0, cx));
    on(&mut ui, &app, "dlgButton", |a, cx| {
        if let Some(i) = cx.key.strip_prefix("dlg-b").and_then(|n| n.parse().ok()) {
            a.press(i, cx);
        }
    });
    on(&mut ui, &app, "dlgPick", |a, cx| {
        let Some(i) = cx.key.strip_prefix("dlg-it").and_then(|n| n.parse::<usize>().ok()) else { return };
        if a.dlg.as_ref().is_some_and(|d| d.items.get(i).is_some_and(|it| it.header)) {
            return;
        }
        if let Some(d) = &mut a.dlg {
            d.sel = i;
            d.moved = true;
            d.write_sel(cx.data);
        }
        if cx.clicks == 2 {
            a.pick(i, cx);
        }
    });
    on(&mut ui, &app, "jobBg", |a, cx| {
        if let Some(j) = &mut a.job {
            j.background = true;
        }
        a.write_job(cx.data);
    });
    on(&mut ui, &app, "jobShow", |a, cx| {
        if let Some(j) = &mut a.job {
            j.background = false;
        }
        a.write_job(cx.data);
    });
    on(&mut ui, &app, "jobCancel", |a, _| {
        if let Some(j) = &a.job {
            j.cancel.cancel();
        }
    });
    on(&mut ui, &app, "viewWheel", App::view_wheel);
    // A name in a panel's title: open that folder.
    on(&mut ui, &app, "crumb", |a, cx| {
        let side = usize::from(cx.key.starts_with("right"));
        let Some(i) = cx.key.split("-cr").nth(1).and_then(|n| n.parse::<usize>().ok()) else { return };
        let Some(uison::ui::Json::Array(cs)) = cx.data.get(&format!("{}.crumbs", if side == 0 { "left" } else { "right" })).cloned() else { return };
        let Some(path) = cs.get(i).and_then(|c| c.get("p")).and_then(|p| p.as_str()).filter(|p| !p.is_empty()) else { return };
        let (dir, from) = (VPath::new(path), a.panels[side].dir.clone());
        a.set_active(side, cx);
        // Up from a deeper folder: the cursor lands on the one we came out of.
        let focus = from.strip_prefix(&dir).and_then(|rest| rest.split('/').next()).filter(|n| !n.is_empty()).map(str::to_string);
        a.load(side, dir, focus);
    });
    // A column's edge let go (`left-cw-size`): keep that panel's widths; a double click gives the column its width back.
    on(&mut ui, &app, "colw", |a, cx| {
        let Some((s, k)) = cx.key.split_once("-cw-") else { return };
        let side = usize::from(s == "right");
        if cx.clicks == 2
            && let Some(i) = COL_KEYS.iter().position(|c| *c == k)
        {
            cx.data.set(&format!("colw.{s}.{k}"), COLS[i]);
        }
        let w: Vec<f64> = COL_KEYS.iter().map(|k| cx.data.num(&format!("colw.{s}.{k}"))).collect();
        a.cols[side] = <[f64; 4]>::try_from(w).ok();
        a.save_state();
    });
    on(&mut ui, &app, "viewMode", |a, cx| {
        let key = match cx.key {
            "vm-text" => "1",
            "vm-hex" => "3",
            _ => "w",
        };
        a.view_key(key, cx);
    });
    // The error strip of a panel: Retry reads the failed folder again (a dropped connection is made again
    // on the way), × just closes it.
    on(&mut ui, &app, "panelErr", |a, cx| {
        let side = usize::from(cx.key.starts_with("right"));
        let p = &mut a.panels[side];
        let failed = p.failed.take();
        p.error = None;
        p.write_status(cx.data, side == a.active);
        if cx.key.ends_with("-retry")
            && let Some((src, dir)) = failed
        {
            a.load_from(side, src, dir, None);
        }
    });
    on(&mut ui, &app, "toastOff", |a, cx| {
        a.toast += 1;
        cx.data.set("toast.on", false);
    });
    for (action, c) in [
        ("ctxOpen", Cmd::Open),
        ("ctxView", Cmd::View),
        ("ctxEdit", Cmd::Edit),
        ("ctxCopy", Cmd::Copy),
        ("ctxMove", Cmd::Move),
        ("ctxRename", Cmd::Rename),
        ("ctxDelete", Cmd::Delete),
        ("ctxProps", Cmd::Properties),
        ("ctxPath", Cmd::CopyPaths),
    ] {
        on(&mut ui, &app, action, move |a, cx| a.context(c.clone(), cx));
    }
    for i in 0..ACTION_SLOTS {
        on(&mut ui, &app, &format!("ctxAct{i}"), move |a, cx| {
            if let Some((side, row)) = row_key(cx.key) {
                a.set_active(side, cx);
                if !a.panels[side].has_selection() {
                    a.panels[side].move_cursor(row as isize);
                    a.moved(side, cx);
                }
            }
            let side = a.active;
            a.source_action(side, i, cx);
        });
    }
    ui
}
