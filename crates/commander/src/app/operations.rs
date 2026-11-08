//! Copy, move, delete: the dialogs before them, the job, its questions and its end.

use super::*;

impl App {
    /// F5 / F6 / Shift+F5 / Shift+F6: the dialog with the target path.
    pub(super) fn ask_transfer(&mut self, mode: Mode, here: bool, cx: &mut Cx) {
        let a = self.active;
        let targets = self.panels[a].targets();
        if targets.is_empty() {
            return;
        }
        let dst_side = if here { a } else { self.other() };
        if (mode == Mode::Move && !self.writable(a, cx)) || !self.writable(dst_side, cx) {
            return;
        }
        let names: Vec<String> = targets.iter().map(|e| e.name.clone()).collect();
        let what = what_of(&targets);
        let (title, verb) = match (mode, here) {
            (Mode::Copy, _) => (t!("Copy"), t!("Copy")),
            (Mode::Move, false) => (t!("Move"), t!("Move")),
            (Mode::Move, true) => (t!("Rename"), t!("Rename")),
        };
        let dst = &self.panels[dst_side];
        // Several objects here: a mask, as in TC (one name for all would mean "into a folder of that name").
        let value = if here && names.len() > 1 {
            "*.*".to_string()
        } else if here {
            names[0].clone()
        } else if dst.dir.is_root() {
            "/".to_string()
        } else {
            format!("{}/", dst.dir)
        };
        let text = if here {
            t!("{} {} to:\nA new name, or a mask such as *.bak", verb, what)
        } else {
            t!("{} {} to {}:\nA folder, a new name, or a mask such as *.bak", verb, what, dst.src.info().title)
        };
        let act = Act::Transfer { mode, side: a, names, here };
        self.open_dialog(Dialog::input(title, &text, &value, if mode == Mode::Copy { t!("Copy") } else { title }, act), cx);
    }

    pub(super) fn start_transfer(&mut self, mode: Mode, side: usize, names: Vec<String>, target: String, here: bool, cx: &mut Cx) {
        let (src_p, dst_side) = (&self.panels[side], if here { side } else { 1 - side });
        let dst_p = &self.panels[dst_side];
        // A relative path counts from the target panel's folder (Shift+F5/F6: this panel's).
        let target = if target.starts_with('/') { target } else { format!("{}/{}", dst_p.dir, target) };
        let focus_name = VPath::new(&target).name().to_string();
        let t = Transfer {
            mode,
            src: src_p.src.clone(),
            src_dir: src_p.dir.clone(),
            names: names.clone(),
            dst: dst_p.src.clone(),
            target: target.clone(),
        };
        let what = if names.len() == 1 { names[0].clone() } else { format::count(names.len() as u64, "object") };
        let title = if mode == Mode::Copy { t!("Copying {}", what) } else { t!("Moving {}", what) };
        let focus = if here && names.len() == 1 { Some((side, focus_name)) } else { None };
        let route = format!("{}: {}  →  {}: {}", t.src.info().title, t.src_dir, t.dst.info().title, t.target);
        self.start_job(title, Some(side), focus, cx, move |fb, cancel| t.run(fb, cancel));
        if let Some(j) = &mut self.job {
            j.route = route;
        }
    }

    pub(super) fn ask_delete(&mut self, cx: &mut Cx) {
        let a = self.active;
        let targets = self.panels[a].targets();
        if targets.is_empty() || !self.writable(a, cx) {
            return;
        }
        // What goes, by name (the first few), so a wrong selection is seen before it is too late.
        let mut list: Vec<String> =
            targets.iter().take(5).map(|e| if e.is_dir() { format!("  {}/", e.name) } else { format!("  {}", e.name) }).collect();
        if targets.len() > 5 {
            list.push(format!("  {}", t!("… and {} more", targets.len() - 5)));
        }
        let text = t!("Delete {}?\n{}\n\nThere is no trash yet: this cannot be undone.", what_of(&targets), list.join("\n"));
        let names = targets.into_iter().map(|e| e.name).collect();
        let d = Dialog::new(Kind::Message, t!("Delete"), &text)
            .buttons(vec![Button::new(t!("Delete"), "danger", Act::Delete { side: a, names }), Button::cancel()]);
        self.open_dialog(d, cx);
    }

    /// Run an operation in a worker; its progress, questions and end come through the mailbox.
    pub(super) fn start_job(
        &mut self,
        title: String,
        deselect: Option<usize>,
        focus: Option<(usize, String)>,
        cx: &mut Cx,
        f: impl FnOnce(&JobFeedback, &Cancel) -> ops::Report + Send + 'static,
    ) {
        if self.job.is_some() {
            self.open_dialog(Dialog::message(t!("Busy"), t!("Another operation is running. Wait for it or cancel it.")), cx);
            return;
        }
        let id = self.next_seq();
        let (fb, answer) = JobFeedback::new(id, self.mb.clone());
        let cancel = Cancel::new();
        let c = cancel.clone();
        spawn(move || {
            let report = f(&fb, &c);
            fb.mb.send(Msg::Done { job: id, report });
        });
        self.job = Some(Job {
            id,
            title,
            cancel,
            answer,
            background: false,
            asking: false,
            last: Progress::default(),
            route: String::new(),
            samples: std::collections::VecDeque::new(),
            focus,
            deselect,
        });
        self.write_job(cx.data);
    }

    pub(super) fn write_job(&self, d: &mut Data) {
        let Some(j) = &self.job else {
            d.set("job.show", false);
            d.set("job.strip", false);
            return;
        };
        let p = &j.last;
        let frac = |a: u64, b: u64| if b == 0 { 0.0 } else { (a as f64 / b as f64).min(1.0) };
        d.set("job.title", j.title.as_str());
        d.set("job.current", if p.planning { t!("Counting… {}", p.current) } else { p.current.clone() });
        d.set("job.fileFrac", frac(p.file_done, p.file_size));
        // Bytes when there are any; otherwise objects (delete, empty files).
        let all = if p.bytes_total > 0 { frac(p.bytes_done, p.bytes_total) } else { frac(p.files_done, p.files_total) };
        d.set("job.allFrac", all);
        // Speed over the last seconds (a question that waited doesn't drag it down), and the time left at it.
        let speed = match (j.samples.front(), j.samples.back()) {
            (Some((t0, b0)), Some((t1, b1))) if t1.duration_since(*t0).as_secs_f64() > 0.3 => {
                Some((b1 - b0) as f64 / t1.duration_since(*t0).as_secs_f64())
            }
            _ => None,
        };
        let mut stats = if p.bytes_total > 0 {
            let files = crate::i18n::of_count(p.files_done, p.files_total, "file");
            format!("{}  ·  {files}", t!("{} of {}", format::short(p.bytes_done), format::short(p.bytes_total)))
        } else {
            crate::i18n::of_count(p.files_done, p.files_total, "object")
        };
        if let Some(v) = speed.filter(|v| *v > 0.0 && !p.planning) {
            stats.push_str(&format!("  ·  {}", t!("{}/s", format::short(v as u64))));
            if p.bytes_total > p.bytes_done {
                let left = format::duration(((p.bytes_total - p.bytes_done) as f64 / v) as u64 + 1);
                stats.push_str(&format!("  ·  {}", t!("{} left", left)));
            }
        }
        d.set("job.route", j.route.as_str());
        d.set("job.stats", stats);
        d.set("job.show", !j.background && !j.asking);
        d.set("job.strip", j.background);
    }

    pub(super) fn job_done(&mut self, report: ops::Report, cx: &mut Cx) {
        let Some(job) = self.job.take() else { return };
        self.write_job(cx.data);
        if let Some(side) = job.deselect
            && !report.cancelled
        {
            for it in &mut self.panels[side].items {
                it.sel = false;
            }
        }
        // Re-read both panels; the cursor goes to what was made, if it is in one of them.
        for side in 0..2 {
            let focus = job.focus.as_ref().filter(|(s, _)| *s == side).map(|(_, n)| n.clone());
            let dir = self.panels[side].dir.clone();
            self.load(side, dir, focus);
        }
        if !report.errors.is_empty() {
            let mut text: Vec<String> = report.errors.iter().take(12).map(|e| crate::i18n::error(e)).collect();
            if report.errors.len() > 12 {
                text.push(t!("… and {} more", report.errors.len() - 12));
            }
            let title = t!("{}: {} done, {} skipped, {} failed", job.title, report.done, report.skipped, report.failed);
            self.notify(Dialog::message(&title, &text.join("\n")), cx);
        } else if report.cancelled {
            self.toast(&t!("{}: cancelled ({} done)", job.title, report.done), cx);
        } else if job.background {
            // A background job ends quietly otherwise: say so.
            self.toast(&t!("{}: done", job.title), cx);
        }
        if self.quitting {
            self.quit();
        }
    }

    /// A question of the running operation.
    pub(super) fn job_ask(&mut self, q: Question, cx: &mut Cx) {
        if let Some(j) = &mut self.job {
            j.asking = true;
        }
        let mut d = match q {
            Question::Exists { path, src, dst } => {
                // Which one is newer, which is larger: said, not left to compare by eye.
                let newer = match (src.modified, dst.modified) {
                    (Some(a), Some(b)) if a > b => (t!(" (newer)"), ""),
                    (Some(a), Some(b)) if a < b => ("", t!(" (newer)")),
                    _ => ("", ""),
                };
                let larger = match (src.size, dst.size) {
                    (Some(a), Some(b)) if a > b => (t!(" (larger)"), ""),
                    (Some(a), Some(b)) if a < b => ("", t!(" (larger)")),
                    _ => ("", ""),
                };
                let line = |e: &Entry, n: &str, l: &str| {
                    let size = e.size.map(|s| format!("{}{l}", format::count(s, "byte"))).unwrap_or_default();
                    format!("{size}  ·  {}{n}", e.modified.map(format::date_long).unwrap_or_default())
                };
                let text = t!("{}\n\nCopying:     {}\nAlready there:  {}", path, line(&src, newer.0, larger.0), line(&dst, newer.1, larger.1));
                Dialog::new(Kind::Message, t!("The file already exists"), &text).buttons(vec![
                    Button::new(t!("Overwrite"), "primary", Act::Answer(Answer::Overwrite)),
                    Button::new(t!("Overwrite all"), "secondary", Act::Answer(Answer::OverwriteAll)),
                    Button::new(t!("Overwrite older"), "secondary", Act::Answer(Answer::OverwriteOlder)),
                    Button::new(t!("Skip"), "secondary", Act::Answer(Answer::Skip)),
                    Button::new(t!("Skip all"), "secondary", Act::Answer(Answer::SkipAll)),
                    Button::new(t!("Rename…"), "secondary", Act::AskRename),
                    Button::new(t!("Cancel"), "secondary", Act::Answer(Answer::Cancel)),
                ])
            }
            Question::Failed { error, .. } => Dialog::new(Kind::Message, t!("Something failed"), &crate::i18n::error(&error)).buttons(vec![
                Button::new(t!("Retry"), "primary", Act::Answer(Answer::Retry)),
                Button::new(t!("Skip"), "secondary", Act::Answer(Answer::Skip)),
                Button::new(t!("Skip all"), "secondary", Act::Answer(Answer::SkipAll)),
                Button::new(t!("Cancel"), "secondary", Act::Answer(Answer::Cancel)),
            ]),
        };
        d.question = true;
        self.question = Some(d.clone());
        self.open_dialog(d, cx);
    }

    pub(super) fn answer(&mut self, a: Answer, cx: &mut Cx) {
        if let Some(j) = &mut self.job {
            j.asking = false;
            let _ = j.answer.send(a);
        }
        self.question = None;
        self.write_job(cx.data);
        // What came while the question was open.
        if self.dlg.is_none() && !self.queued.is_empty() {
            let d = self.queued.remove(0);
            self.open_dialog(d, cx);
        }
    }

    pub(super) fn count_size(&mut self, side: usize, name: String) {
        let p = &mut self.panels[side];
        let (src, dir, mb, cancel) = (p.src.clone(), p.dir.clone(), self.mb.clone(), Cancel::new());
        p.sizing.retain(|c| !c.is_cancelled());
        p.sizing.push(cancel.clone());
        spawn(move || {
            let res = ops::dir_size(&*src, &dir.child(&name), &cancel);
            if res != Err(vfs::Error::Cancelled) {
                mb.send(Msg::Size { side, src: src.info().id, dir, name, res });
            }
        });
    }
}
