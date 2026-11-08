//! The viewer (F3).

use super::*;

impl App {
    pub(super) fn view_file(&mut self, cx: &mut Cx) {
        let seq = self.next_seq();
        let p = &self.panels[self.active];
        let Some(e) = p.current().filter(|e| !e.is_dir()).cloned() else { return };
        let size = e.size.unwrap_or(0);
        let image = viewer::is_image(&e.name) && size <= viewer::MAX_IMAGE;
        let mut v = Viewer::new(p.src.clone(), p.dir.child(&e.name), size, image, seq);
        self.geometry(&mut v, cx.data);
        if image {
            let (src, path, mb) = (v.src.clone(), v.path.clone(), self.mb.clone());
            spawn(move || {
                let res = (|| {
                    let mut buf = Vec::new();
                    std::io::Read::read_to_end(&mut src.open_read(&path, 0)?, &mut buf).map_err(|e| vfs::Error::io(&path, &e))?;
                    Ok(buf)
                })();
                mb.send(Msg::Image { seq, res });
            });
        }
        cx.data.set("view.title", format!("{}: {}", v.src.info().title, v.path));
        cx.data.set("view.isImg", image);
        cx.data.set("view.img", "");
        cx.data.set(
            "view.help",
            if image {
                t!("Esc close")
            } else {
                t!("Esc close  ·  1 text  ·  3 hex  ·  W wrap  ·  F7 search  ·  F3 / Shift+F3 next / previous")
            },
        );
        self.view = Some(v);
        cx.data.set("view.open", true);
        self.view_update(cx.data);
    }

    pub(super) fn geometry(&self, v: &mut Viewer, d: &Data) {
        let (w, h) = (d.num("env.width") as f32, d.num("env.height") as f32);
        v.rows = (((h - VIEW_CHROME_H) / VIEW_LINE_H).floor() as usize).max(1);
        // Menlo 12: 7.2 pt per character.
        v.cols = (((w - 16.0) / 7.22).floor() as usize).max(8);
    }

    /// The visible lines; reads the next window when the view nears the edge of the loaded one.
    pub(super) fn view_update(&mut self, d: &mut Data) {
        let Some(v) = &mut self.view else { return };
        if let Some(off) = v.need() {
            v.pending = Some(off);
            let (src, path, mb, seq) = (v.src.clone(), v.path.clone(), self.mb.clone(), v.seq);
            spawn(move || {
                let res = (|| {
                    let mut r = src.open_read(&path, off)?;
                    let mut buf = Vec::with_capacity(viewer::WINDOW as usize);
                    std::io::Read::read_to_end(&mut std::io::Read::take(&mut r, viewer::WINDOW), &mut buf).map_err(|e| vfs::Error::io(&path, &e))?;
                    Ok(buf)
                })();
                mb.send(Msg::View { seq, offset: off, res });
            });
        }
        let lines: Vec<Json> = v.lines().into_iter().map(Json::from).collect();
        d.set("view.lines", Json::Array(lines));
        d.set("view.pos", v.position());
        d.set("view.text", v.mode == ViewMode::Text);
        d.set("view.hex", v.mode == ViewMode::Hex);
        d.set("view.wrap", v.wrap);
    }

    pub(super) fn view_key(&mut self, key: &str, cx: &mut Cx) {
        let Some(v) = &mut self.view else { return };
        let page = v.rows.saturating_sub(1).max(1);
        match key {
            "escape" | "f3" if v.search.is_empty() || key == "escape" => {
                if key == "f3" && !v.search.is_empty() {
                    return self.view_next(false);
                }
                return self.close_view(cx);
            }
            "q" => return self.close_view(cx),
            "f3" => return self.view_next(false),
            "shift+f3" => return self.view_next(true),
            "f7" => {
                let s = v.search.clone();
                self.open_dialog(Dialog::input(t!("Search"), t!("Text (any case):"), &s, t!("Find"), Act::ViewSearch), cx);
                return;
            }
            "down" => v.down(1),
            "up" => v.up(1),
            "pagedown" | "space" => v.down(page),
            "pageup" => v.up(page),
            "home" | "ctrl+home" | "cmd+up" => v.home(),
            "end" | "ctrl+end" | "cmd+down" => v.end(),
            "1" => v.set_mode(ViewMode::Text),
            "3" | "h" => v.set_mode(ViewMode::Hex),
            "w" => v.wrap = !v.wrap,
            _ => return,
        }
        v.note.clear();
        self.view_update(cx.data);
    }

    pub(super) fn view_wheel(&mut self, cx: &mut Cx) {
        let Some(v) = &mut self.view else { return };
        let dy = cx.wheel.map_or(0.0, |w| w.1);
        let lines = (dy.abs() / VIEW_LINE_H).ceil() as usize;
        if dy < 0.0 {
            v.down(lines.max(1));
        } else if dy > 0.0 {
            v.up(lines.max(1));
        }
        self.view_update(cx.data);
    }

    pub(super) fn view_search(&mut self, text: String, back: bool) {
        if let Some(v) = &mut self.view {
            v.search = text;
            v.found = None;
        }
        self.view_next(back);
    }

    /// Esc, F3, q: the viewer goes, and its search with it.
    pub(super) fn close_view(&mut self, cx: &mut Cx) {
        if let Some(c) = self.view.take().and_then(|v| v.searching) {
            c.cancel();
        }
        cx.data.set("view.open", false);
        cx.data.set("view.lines", Json::Array(Vec::new()));
    }

    pub(super) fn view_next(&mut self, back: bool) {
        let Some(v) = &mut self.view else { return };
        // One search at a time: a new one stops the last, whose answer would land out of order.
        if let Some(c) = v.searching.take() {
            c.cancel();
        }
        let cancel = Cancel::new();
        v.searching = Some(cancel.clone());
        let from = match (v.found, back) {
            (Some(f), false) => f + 1,
            (Some(f), true) => f,
            (None, false) => v.top,
            (None, true) => v.top,
        };
        v.note = t!("searching…").into();
        let (src, path, size, seq, mb) = (v.src.clone(), v.path.clone(), v.size, v.seq, self.mb.clone());
        let needle = v.search.clone().into_bytes();
        spawn(move || {
            let res = viewer::find(&*src, &path, size, &needle, from, back, &cancel);
            if res == Err(vfs::Error::Cancelled) {
                return;
            }
            mb.send(Msg::Found { seq, res });
        });
    }

    pub(super) fn edit_file(&mut self) {
        let p = &self.panels[self.active];
        let Some(e) = p.current().filter(|e| !e.is_dir()) else { return };
        // The system's editor needs a file on this machine.
        let Some(path) = p.src.local_path(&p.dir.child(&e.name)) else { return };
        open_in_editor(&path);
    }
}

/// The system's text editor; F4 in TC opens an external editor too.
pub(super) fn open_in_editor(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-t").arg(path).spawn();
    #[cfg(not(target_os = "macos"))]
    let _ = path;
}
