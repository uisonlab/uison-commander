//! The mailbox: what workers sent, applied on the UI thread.

use super::*;

impl App {
    pub(super) fn pump(&mut self, cx: &mut Cx) {
        let mut job_changed = false;
        let mut view_changed = false;
        for m in self.mb.take() {
            match m {
                Msg::Listed { side, seq, src, dir, res, focus, space } => {
                    let p = &mut self.panels[side];
                    if p.seq != seq {
                        continue;
                    }
                    p.loading = None;
                    let same = same_storage(&*src, &*p.src);
                    match res {
                        Ok(entries) => {
                            if p.dir != dir || p.src.info().id != src.info().id {
                                p.remember((p.src.clone(), p.dir.clone()));
                            }
                            p.src = src;
                            p.space = space;
                            p.set_listing(dir, entries, focus.as_deref());
                            self.write_panel(side, cx.data);
                            self.moved(side, cx);
                            self.save_state();
                        }
                        // The folder itself is gone (deleted from elsewhere): up to what exists.
                        Err(vfs::Error::NotFound(_)) if same && dir == p.dir && !dir.is_root() => {
                            let up = dir.parent().unwrap_or_else(VPath::root);
                            self.load(side, up, None);
                        }
                        // In the panel, with Retry: no window to close, and nothing else is in the way.
                        Err(e) => {
                            p.error = Some(t!("Can't open {}: {}", src.info().title, crate::i18n::err(&e)));
                            p.failed = Some((src, dir));
                            p.write_status(cx.data, side == self.active);
                        }
                    }
                }
                Msg::Slow { side, seq } => {
                    let p = &self.panels[side];
                    if p.seq == seq && p.loading.is_some() {
                        p.write_status(cx.data, side == self.active);
                    }
                }
                Msg::Size { side, src, dir, name, res } => {
                    let p = &mut self.panels[side];
                    if p.dir != dir || p.src.info().id != src {
                        continue;
                    }
                    if let Ok(s) = res {
                        p.sizes.insert(name.clone(), s.bytes);
                        if let Some(r) = p.find(&name) {
                            p.write_row(cx.data, r - p.up_row());
                        }
                        p.write_status(cx.data, side == self.active);
                    }
                }
                Msg::Roots { seq, index, res } => {
                    if let Some(r) = &mut self.roots
                        && r.seq == seq
                        && let Some(slot) = r.results.get_mut(index)
                    {
                        *slot = Some(res);
                        self.show_roots(cx, false);
                    }
                }
                Msg::Progress { job, p } => {
                    if let Some(j) = self.job.as_mut().filter(|j| j.id == job) {
                        let now = Instant::now();
                        j.samples.push_back((now, p.bytes_done));
                        while j.samples.front().is_some_and(|(t, _)| now.duration_since(*t).as_secs_f64() > 5.0) {
                            j.samples.pop_front();
                        }
                        j.last = p;
                        job_changed = true;
                    }
                }
                Msg::Ask { job, q } => {
                    if self.job.as_ref().is_some_and(|j| j.id == job) {
                        // A question over another dialog (the user opened one meanwhile): that one comes back after.
                        if let Some(mut d) = self.dlg.take() {
                            // What was typed comes back with it (a connection form half filled in).
                            if d.kind == Kind::Params {
                                d.typed = Some(d.args(cx.data));
                            }
                            Dialog::clear(cx.data);
                            self.queued.insert(0, d);
                        }
                        self.job_ask(q, cx);
                        job_changed = true;
                    }
                }
                Msg::Done { job, report } => {
                    if self.job.as_ref().is_some_and(|j| j.id == job) {
                        self.job_done(report, cx);
                    }
                }
                Msg::Quit => self.quit(),
                Msg::ToastOff { n } => {
                    if n == self.toast {
                        cx.data.set("toast.on", false);
                    }
                }
                Msg::Mkdir { side, res } => match res {
                    Ok(made) => {
                        let dir = self.panels[side].dir.clone();
                        self.load(side, dir, Some(made.name().to_string()));
                        let o = 1 - side;
                        if self.panels[o].dir == self.panels[side].dir {
                            self.refresh(o);
                        }
                    }
                    Err(e) => self.notify(Dialog::message(t!("Cannot create the folder"), &crate::i18n::err(&e)), cx),
                },
                Msg::NewFile { side, res } => match res {
                    Ok(path) => {
                        let src = self.panels[side].src.clone();
                        let dir = self.panels[side].dir.clone();
                        self.load(side, dir, Some(path.name().to_string()));
                        if let Some(os) = src.local_path(&path) {
                            super::view::open_in_editor(&os);
                        }
                    }
                    Err(e) => self.notify(Dialog::message(t!("Cannot create the file"), &crate::i18n::err(&e)), cx),
                },
                Msg::ActionReady { side, targets, res } => match res {
                    // Something else was opened meanwhile: that wins.
                    _ if self.dlg.is_some() || self.view.is_some() => {}
                    Ok(action) => self.action_dialog(side, action, targets, cx),
                    Err(e) => self.open_dialog(Dialog::message(t!("Action failed"), &crate::i18n::err(&e)), cx),
                },
                Msg::Connected { side, seq, res, note, profile, dir, quiet } => {
                    // The connection form's answer: the form says what went wrong (or that a test worked) and
                    // stays; a connection that worked closes it and goes on as any other.
                    let form = self.form.as_ref().filter(|f| f.seq == Some(seq)).map(|f| (f.test, f.edit));
                    if let Some((test, edit)) = form {
                        match &res {
                            Err(e) => {
                                self.form_status(Status::Error, crate::i18n::err(e), cx);
                                continue;
                            }
                            Ok(s) if test => {
                                let mut t = t!("It works: connected to {}.", s.info().title);
                                if let Some(n) = &note {
                                    t = format!("{t} {n}");
                                }
                                self.form_status(Status::Ok, t, cx);
                                continue;
                            }
                            Ok(_) => {
                                self.close_form(cx);
                                // Changed into another connection: the old one goes, and its secrets.
                                let old = edit.and_then(|i| self.profiles.get(i)).cloned();
                                if let (Some(old), Some(new)) = (old, &profile)
                                    && old.id != new.id
                                {
                                    self.profiles.retain(|p| p.id != old.id);
                                    if self.persist {
                                        for k in &old.secrets {
                                            crate::secrets::forget(&old.id, k);
                                        }
                                    }
                                }
                            }
                        }
                    } else if !quiet {
                        // Cancelled meanwhile: forget it.
                        if self.connecting != Some(seq) {
                            continue;
                        }
                        self.cancel_connect(cx);
                    }
                    if let Some(p) = profile {
                        match self.profiles.iter().position(|x| x.id == p.id) {
                            Some(i) => self.profiles[i] = p,
                            None => self.profiles.push(p),
                        }
                        // Checks and screenshots (`Start::Fixed`) leave the user's files alone.
                        if self.persist {
                            crate::connect::save(&self.profiles);
                        }
                    }
                    match res {
                        Ok(src) => {
                            // The same storage and keys again: take the new one instead of a twin.
                            let id = src.info().id;
                            match self.sources.iter().position(|s| s.info().id == id) {
                                Some(i) => self.sources[i] = src.clone(),
                                None => self.sources.push(src.clone()),
                            }
                            let to = dir.unwrap_or_else(|| src.home());
                            let title = src.info().title;
                            if !quiet {
                                self.set_active(side, cx);
                            }
                            self.load_from(side, src, to, None);
                            if let Some(n) = note {
                                self.toast(&n, cx);
                            } else if !quiet {
                                self.toast(&t!("Connected to {}", title), cx);
                            }
                        }
                        Err(e) => self.notify(Dialog::message(t!("Cannot connect"), &crate::i18n::err(&e)), cx),
                    }
                }
                Msg::Confirm { side, seq, index, mut args, title, text, button, key, value, dir, quiet } => {
                    // In the form: the question is its line, and its Connect button becomes the yes.
                    if self.form.as_ref().is_some_and(|f| f.seq == Some(seq)) {
                        if let Some(f) = &mut self.form {
                            f.trust = Some((key, value));
                        }
                        if let Some((d, open)) = self.form_dialog()
                            && let Some(b) = d.buttons.first_mut()
                        {
                            b.label = button;
                            if open {
                                d.write_buttons(cx.data);
                            }
                        }
                        self.form_status(Status::Ask, format!("{title}. {text}"), cx);
                        continue;
                    }
                    if !quiet {
                        if self.connecting != Some(seq) {
                            continue;
                        }
                        self.cancel_connect(cx);
                    }
                    args.0.retain(|(k, _)| *k != key);
                    args.0.push((key, value));
                    self.confirming = Some((side, index, args, dir));
                    let d = Dialog::new(Kind::Message, &title, &text)
                        .buttons(vec![Button::new(&button, "primary", Act::ConfirmConnect), Button::cancel()]);
                    self.notify(d, cx);
                }
                Msg::Secrets { side, profile, args, dir, quiet } => match args {
                    Some(args) => {
                        let Some(p) = self.profiles.get(profile) else { continue };
                        let Some(index) = crate::connect::all().iter().position(|c| c.id == p.kind) else { continue };
                        self.connect(side, index, args, dir, quiet, cx);
                    }
                    None if !quiet => self.ask_saved(side, profile, false, cx),
                    None => {}
                },
                Msg::Changed { side, res } => {
                    // Re-read even after an error: the permissions may have changed before the rename failed.
                    let dir = self.panels[side].dir.clone();
                    let focus = res.as_ref().ok().cloned();
                    self.load(side, dir, focus);
                    let o = 1 - side;
                    if self.panels[o].dir == self.panels[side].dir && same_storage(&*self.panels[o].src, &*self.panels[side].src) {
                        self.refresh(o);
                    }
                    if let Err(e) = res {
                        self.notify(Dialog::message(t!("Cannot change the properties"), &crate::i18n::err(&e)), cx);
                    }
                }
                Msg::Action { res } => match res {
                    Ok(ActionResult::Clipboard(t)) => {
                        let n = t.lines().count();
                        cx.copy(&t);
                        self.toast(
                            &if n == 1 {
                                t!("Copied to the clipboard").to_string()
                            } else {
                                t!("Copied to the clipboard: {}", format::count(n as u64, "line"))
                            },
                            cx,
                        );
                    }
                    Ok(ActionResult::OpenUrl(u)) => cx.open_url(&u),
                    Ok(ActionResult::Message(m)) => self.toast(&m, cx),
                    Ok(ActionResult::Refresh) => self.refresh_all(),
                    Ok(ActionResult::None) => {}
                    Err(e) => self.notify(Dialog::message(t!("Action failed"), &crate::i18n::err(&e)), cx),
                },
                Msg::View { seq, offset, res } => {
                    if let Some(v) = self.view.as_mut().filter(|v| v.seq == seq) {
                        match res {
                            Ok(b) => {
                                v.loaded(offset, b);
                                v.settle();
                            }
                            Err(e) => {
                                v.pending = None;
                                v.note = crate::i18n::err(&e);
                            }
                        }
                        view_changed = true;
                    }
                }
                Msg::Image { seq, res } => {
                    if let Some(v) = self.view.as_mut().filter(|v| v.seq == seq) {
                        match res {
                            Ok(b) => {
                                let name = format!("view-{seq}");
                                uison::image::put(&name, b);
                                let src = format!("mem:{name}");
                                // Its own size, at most the viewer's area (in points: a 2× screen's pixels are halves).
                                let (aw, ah) = (cx.data.num("env.width") as f32 - 16.0, cx.data.num("env.height") as f32 - VIEW_CHROME_H);
                                let (w, h) = uison::image::natural_size(&src).map_or((aw, ah), |s| (s.w, s.h));
                                let k = (aw / w).min(ah / h).min(1.0);
                                cx.data.set("view.imgW", (w * k) as f64);
                                cx.data.set("view.imgH", (h * k) as f64);
                                cx.data.set("view.img", src);
                                v.note = format!("{} × {} px", w as u32, h as u32);
                                view_changed = true;
                            }
                            Err(e) => {
                                v.image = false;
                                v.note = crate::i18n::err(&e);
                                cx.data.set("view.isImg", false);
                                view_changed = true;
                            }
                        }
                    }
                }
                Msg::Found { seq, res } => {
                    if let Some(v) = self.view.as_mut().filter(|v| v.seq == seq) {
                        match res {
                            Ok(Some(pos)) => {
                                v.note = t!("found at {}", format::bytes(pos));
                                v.show(pos);
                                v.settle();
                            }
                            Ok(None) => v.note = t!("\"{}\" not found", v.search),
                            Err(e) => v.note = crate::i18n::err(&e),
                        }
                        view_changed = true;
                    }
                }
            }
        }
        if job_changed {
            self.write_job(cx.data);
        }
        if view_changed {
            self.view_update(cx.data);
        }
    }
}
