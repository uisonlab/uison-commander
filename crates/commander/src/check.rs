//! Checks without a window: the real app, real events, PNG screenshots (`--png out/`) and tests.
//! Worker threads run for real: after each step the check waits until the mailbox is quiet.

use std::sync::Arc;
use std::time::{Duration, Instant};

use uison::{Event, Headless, Key, KeyCode, Mods, MouseButton, Point};
use vfs::{Source, VPath};

use crate::app::{Start, build};
use crate::demo_source;

pub struct Run {
    pub h: Headless,
    dir: String,
    n: usize,
}

/// `shift+f5`, `ctrl+r`, `a`, `space`, `enter` → a key event.
pub fn key(spec: &str) -> Key {
    let mut mods = Mods::default();
    let mut name = spec;
    while let Some((m, rest)) = name.split_once('+').filter(|(m, r)| !r.is_empty() && ["shift", "ctrl", "alt", "cmd"].contains(m)) {
        match m {
            "shift" => mods.shift = true,
            "ctrl" => mods.ctrl = true,
            "alt" => mods.alt = true,
            _ => mods.cmd = true,
        }
        name = rest;
    }
    let (code, text) = match name {
        "space" => (KeyCode::Char, Some(" ".into())),
        n => match uison::event::key_code(n) {
            Some(c) if n != "char" => (c, None),
            _ => (KeyCode::Char, Some(n.into())),
        },
    };
    Key { code, text, mods }
}

impl Run {
    pub fn new(sources: Vec<Arc<dyn Source>>, start: [(usize, VPath); 2], dir: &str) -> Run {
        let ui = build(sources, Start::Fixed(start));
        let mut r = Run { h: Headless::new(ui, 2.0).expect("headless"), dir: dir.into(), n: 0 };
        r.settle();
        r
    }

    /// The app as it starts for real (state and remembered connections from `UISON_CONFIG_DIR`).
    #[cfg(all(test, feature = "sftp"))]
    pub fn saved(sources: Vec<Arc<dyn Source>>) -> Run {
        let ui = build(sources, Start::Saved(None));
        let mut r = Run { h: Headless::new(ui, 2.0).expect("headless"), dir: String::new(), n: 0 };
        r.settle();
        r
    }

    /// The system's dark (or light) appearance, as the window would get it.
    pub fn theme(&mut self, dark: bool) -> &mut Run {
        let _ = self.h.event(Event::Appearance(dark));
        self
    }

    /// Let workers finish: wake the UI and tick its timers until nothing changes for a while.
    pub fn settle(&mut self) {
        let t0 = Instant::now();
        let mut quiet = 0;
        // Quiet for longer than the delay of "Loading…": a slow read shows itself before the check gives up.
        while quiet < 14 && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(15));
            let a = self.h.event(Event::Wake).ok().flatten().is_some();
            let b = self.h.event(Event::Tick).ok().flatten().is_some();
            // A folder being read is not quiet, however long it takes.
            quiet = if a || b || self.text().contains(t!("Loading…")) { 0 } else { quiet + 1 };
        }
    }

    /// Let time pass with the UI awake (a running operation keeps reporting).
    pub fn wait(&mut self, d: Duration) -> &mut Run {
        let t0 = Instant::now();
        while t0.elapsed() < d {
            std::thread::sleep(Duration::from_millis(20));
            let _ = self.h.event(Event::Wake);
            let _ = self.h.event(Event::Tick);
        }
        self
    }

    /// Let time pass until the screen shows `t` (a slow source's answer), at most `max`.
    #[cfg(all(test, feature = "sftp"))]
    pub fn wait_for(&mut self, t: &str, max: Duration) -> &mut Run {
        let t0 = Instant::now();
        while !self.text().contains(t) {
            assert!(t0.elapsed() < max, "no '{t}' on the screen after {max:?}: {}", self.text());
            self.wait(Duration::from_millis(50));
        }
        self.settle();
        self
    }

    /// Let time pass until `t` is no longer on the screen (an answer that replaces "Connecting…"), at most `max`.
    #[cfg(all(test, feature = "sftp"))]
    pub fn wait_gone(&mut self, t: &str, max: Duration) -> &mut Run {
        let t0 = Instant::now();
        while self.text().contains(t) && t0.elapsed() < max {
            self.wait(Duration::from_millis(50));
        }
        self.settle();
        self
    }

    pub fn keys(&mut self, specs: &str) -> &mut Run {
        for s in specs.split_whitespace() {
            self.h.event(Event::Key(key(s))).unwrap();
            self.settle();
        }
        self
    }

    pub fn typed(&mut self, text: &str) -> &mut Run {
        for c in text.chars() {
            self.h.event(Event::Key(Key::char(&c.to_string()))).unwrap();
        }
        self.settle();
        self
    }

    pub fn click(&mut self, x: f32, y: f32, button: MouseButton) -> &mut Run {
        let p = Point::new(x, y);
        let hit = uison::hit_test(&self.h.rt.frame, p).map(|s| s.to_string());
        let _ = hit;
        self.h.event(Event::MouseMove(p)).unwrap();
        self.h.event(Event::MouseDown(p, button)).unwrap();
        if button != MouseButton::Right {
            self.h.event(Event::MouseUp(p, button)).unwrap();
        }
        self.settle();
        self
    }

    /// Press the element with this key, move by `dx`, let go there.
    #[cfg(test)]
    pub fn drag_key(&mut self, key: &str, dx: f32) -> &mut Run {
        let r = uison::hit_areas(&self.h.rt.frame).into_iter().find(|(id, _)| *id == key).map(|(_, r)| r);
        let r = r.unwrap_or_else(|| panic!("no element '{key}' on the screen"));
        let (x, y) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        self.h.event(Event::MouseMove(Point::new(x, y))).unwrap();
        self.h.event(Event::MouseDown(Point::new(x, y), MouseButton::Left)).unwrap();
        for i in 1..=4 {
            self.h.event(Event::MouseMove(Point::new(x + dx * i as f32 / 4.0, y))).unwrap();
        }
        self.h.event(Event::MouseUp(Point::new(x + dx, y), MouseButton::Left)).unwrap();
        self.settle();
        self
    }

    /// The left edge of a key's element on the screen.
    #[cfg(test)]
    pub fn x_of(&self, key: &str) -> f32 {
        uison::hit_areas(&self.h.rt.frame).into_iter().find(|(id, _)| *id == key).map(|(_, r)| r.x).unwrap_or_else(|| panic!("no '{key}'"))
    }

    /// Click the middle of the element with this key.
    pub fn click_key(&mut self, key: &str, button: MouseButton) -> &mut Run {
        let r = uison::hit_areas(&self.h.rt.frame).into_iter().find(|(id, _)| *id == key).map(|(_, r)| r);
        let r = r.unwrap_or_else(|| panic!("no element '{key}' on the screen"));
        self.click(r.x + r.w / 2.0, r.y + r.h / 2.0, button)
    }

    /// Click the middle of a text on the screen (a row by its name).
    pub fn click_text(&mut self, t: &str, button: MouseButton) -> &mut Run {
        fn find(items: &[uison::Item], t: &str) -> Option<uison::Rect> {
            items.iter().find_map(|it| match &it.shape {
                uison::Shape::Text { text, .. } if text.as_ref() == t => Some(it.bounds),
                uison::Shape::Clip { items, .. } => find(items, t),
                _ => None,
            })
        }
        let r = find(&self.h.rt.frame.items, t).unwrap_or_else(|| panic!("no text '{t}' on the screen"));
        self.click(r.x + r.w / 2.0, r.y + r.h / 2.0, button)
    }

    /// Click `t` that comes after the text `after` (a row under its group title in a list).
    pub fn click_text_after(&mut self, after: &str, t: &str, button: MouseButton) -> &mut Run {
        fn all(items: &[uison::Item], out: &mut Vec<(String, uison::Rect)>) {
            for it in items {
                match &it.shape {
                    uison::Shape::Text { text, .. } => out.push((text.to_string(), it.bounds)),
                    uison::Shape::Clip { items, .. } => all(items, out),
                    _ => {}
                }
            }
        }
        let mut texts = Vec::new();
        all(&self.h.rt.frame.items, &mut texts);
        let top = texts.iter().find(|(x, _)| x == after).map(|(_, r)| r.y).unwrap_or_else(|| panic!("no text '{after}' on the screen"));
        let r = texts.iter().filter(|(x, r)| x == t && r.y > top).min_by(|a, b| a.1.y.total_cmp(&b.1.y)).map(|(_, r)| *r);
        let r = r.unwrap_or_else(|| panic!("no text '{t}' after '{after}'"));
        self.click(r.x + r.w / 2.0, r.y + r.h / 2.0, button)
    }

    pub fn shot(&mut self, name: &str) -> &mut Run {
        self.n += 1;
        if !self.dir.is_empty() {
            let path = format!("{}/{:02}_{name}.png", self.dir, self.n);
            self.h.save_png(&path, false).unwrap();
        }
        self
    }

    /// All text of the frame, for assertions.
    pub fn text(&self) -> String {
        fn walk(items: &[uison::Item], out: &mut String) {
            for it in items {
                match &it.shape {
                    uison::Shape::Text { text, .. } => {
                        out.push_str(text);
                        out.push('\n');
                    }
                    uison::Shape::Clip { items, .. } => walk(items, out),
                    _ => {}
                }
            }
        }
        let mut s = String::new();
        walk(&self.h.rt.frame.items, &mut s);
        // Each panel's title as one line too: on the screen it is pieces (the path's names are clickable).
        for side in ["left", "right"] {
            let Some(area) = uison::hit_areas(&self.h.rt.frame).into_iter().find(|(id, _)| *id == format!("{side}-hdr")).map(|(_, r)| r) else {
                continue;
            };
            let mut parts: Vec<(f32, String)> = Vec::new();
            fn inside(items: &[uison::Item], a: uison::Rect, out: &mut Vec<(f32, String)>) {
                for it in items {
                    match &it.shape {
                        uison::Shape::Text { text, .. }
                            if it.bounds.y >= a.y && it.bounds.y < a.y + a.h && it.bounds.x >= a.x && it.bounds.x < a.x + a.w =>
                        {
                            out.push((it.bounds.x, text.to_string()))
                        }
                        uison::Shape::Clip { items, .. } => inside(items, a, out),
                        _ => {}
                    }
                }
            }
            inside(&self.h.rt.frame.items, area, &mut parts);
            parts.sort_by(|a, b| a.0.total_cmp(&b.0));
            s.push_str(&parts.into_iter().map(|p| p.1).collect::<String>());
            s.push('\n');
        }
        s
    }
}

/// The screenshots of `--png`: the demo source in both panels, then the main dialogs — in the light theme
/// in `dir`, in the dark one in `dir/dark`, in Russian in `dir/ru`.
pub fn png(dir: &str) {
    png_in(dir, false);
    png_in(&format!("{dir}/dark"), true);
    crate::i18n::set(crate::i18n::Lang::Ru);
    png_in(&format!("{dir}/ru"), false);
    crate::i18n::set(crate::i18n::Lang::En);
}

/// The screenshots in the current language (`dir` empty: the same steps, no files).
fn png_in(dir: &str, dark: bool) {
    // The folder for the screenshots, every level, through the source like any folder.
    let mut d = VPath::root();
    for c in vfs_local::vpath(std::path::Path::new(dir)).components().filter(|_| !dir.is_empty()) {
        d = d.child(c);
        let _ = vfs_local::LocalFs::new().mkdir(&d);
    }
    let mem: Arc<dyn Source> = Arc::new(demo_source(0));
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), mem];
    let mut r = Run::new(sources, [(1, VPath::root()), (1, VPath::new("/docs"))], dir);
    r.theme(dark);
    r.shot("start");
    r.keys("f1").shot("help");
    r.keys("escape");
    r.keys("down down insert down insert").shot("selected");
    r.keys("f5").shot("copy_dialog");
    r.keys("enter").shot("copied");
    r.keys("tab down down down down f3").shot("viewer_text");
    r.keys("escape tab home enter").shot("big_folder");
    r.keys("down f3").shot("viewer_log");
    r.keys("w pagedown").shot("viewer_log_paged");
    r.keys("3").shot("viewer_hex");
    r.keys("escape alt+f1").shot("roots");
    r.keys("escape alt+enter").shot("properties");
    r.click_key("dlg-perm-gw", MouseButton::Left).shot("properties_ticked");
    r.keys("escape f8").shot("delete");
    r.keys("escape f7").typed("new/deep").shot("mkdir");
    r.keys("enter").shot("mkdir_done");
    r.keys("s").shot("quick_search");
    r.click_key("left-r1", MouseButton::Right).shot("context_menu");
    r.keys("escape escape home down alt+enter");
    r.click_text(&format!("{}…", t!("Copy link")), MouseButton::Left).keys("enter").shot("toast");
    r.click_key("tb-help", MouseButton::Left).shot("help_menu");
    r.click_text(t!("About uison-commander"), MouseButton::Left).shot("about");
    r.keys("escape");
    r.click_key("tb-settings", MouseButton::Left).shot("settings");
    r.keys("escape");
    // Connections: the menu, a new SFTP connection that fails — in its form, with the reason; the list.
    if crate::connect::all().iter().any(|c| c.id == "sftp") {
        r.keys("ctrl+f").shot("connect_menu");
        r.click_text("SFTP", MouseButton::Left).shot("connect_form");
        r.typed("me@127.0.0.1:1").keys("enter").wait(Duration::from_millis(500));
        r.settle();
        r.shot("connect_failed");
        r.keys("escape");
        r.keys("ctrl+f").click_text(t!("Edit…"), MouseButton::Left).shot("connections");
        r.keys("escape");
    }
    // S3: Amazon asks a name and the keys; a compatible storage, its address too.
    if crate::connect::all().iter().any(|c| c.id == "s3") {
        r.keys("ctrl+f").click_text("Amazon S3", MouseButton::Left).shot("connect_s3");
        r.keys("escape");
        r.keys("ctrl+f").click_text(t!("S3-compatible storage"), MouseButton::Left).shot("connect_s3_compatible");
        r.keys("escape");
    }
    // A source whose root can't be read: the error strip in the panel.
    let broken = vfs::MemFs::new("broken");
    broken.fail_on("/");
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), Arc::new(demo_source(0)), Arc::new(broken)];
    let mut b = Run::new(sources, [(1, VPath::root()), (1, VPath::new("/docs"))], dir);
    b.theme(dark);
    b.n = r.n;
    b.keys("alt+f1");
    b.click_text_after("mem:broken", "/", MouseButton::Left).shot("panel_error");
    // A real picture in the viewer (a system symbol drawn large, where there is one).
    let pics = demo_source(0);
    if let Some(png) = uison::image::bytes(&crate::icons::symbol("photo.on.rectangle", dark)) {
        pics.add_file("/photos/picture.png", png.to_vec());
    }
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), Arc::new(pics)];
    let mut v = Run::new(sources, [(1, VPath::new("/photos")), (1, VPath::new("/docs"))], dir);
    v.theme(dark);
    v.n = b.n;
    v.keys("end f3").wait(Duration::from_millis(300));
    v.settle();
    v.shot("viewer_image");
    r.n = v.n;
    // A slow source (every 1 MB block takes 30 ms): the progress dialog, then a conflict.
    let slow: Arc<dyn Source> = Arc::new(demo_source(30));
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), slow];
    let mut s = Run::new(sources, [(1, VPath::new("/big")), (1, VPath::new("/docs"))], dir);
    s.theme(dark);
    s.n = r.n;
    s.keys("end f5");
    s.h.event(Event::Key(key("enter"))).unwrap();
    s.wait(Duration::from_millis(900)).shot("progress");
    s.settle();
    s.keys("f5 enter").shot("conflict");
    s.keys("escape");
    if !dir.is_empty() {
        println!("{} screenshots in {dir}", s.n);
    }
}

/// `--bench folder`: how long the folder takes to show, and keys in it.
pub fn bench(folder: VPath) {
    let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new())];
    let t0 = Instant::now();
    let mut r = Run::new(sources, [(0, folder), (0, VPath::root())], "");
    println!("open (until the listing is shown): {} ms", t0.elapsed().as_millis());
    let mut times = Vec::new();
    for k in ["down", "end", "home", "pagedown", "insert", "pageup", "ctrl+f6", "ctrl+f3"] {
        let t = Instant::now();
        r.h.event(Event::Key(key(k))).unwrap();
        times.push((k, t.elapsed().as_micros()));
        let first = r.text().lines().find(|l| l.starts_with("file_")).unwrap_or("").to_string();
        println!("  after {k}: first visible {first}");
    }
    for (k, us) in times {
        println!("  {k:>9}: {:.1} ms", us as f64 / 1000.0);
    }
    r.keys("end");
    let t = r.text();
    let last = t.lines().rfind(|l| l.starts_with("file_")).unwrap_or("none");
    println!("after End the last row is {last}; status: {}", t.lines().find(|l| l.contains(" files")).unwrap_or(""));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The group of the local disk in the source list (`source_title`).
    const DISK: &str = if cfg!(target_os = "macos") { "This Mac" } else { "This computer" };

    fn demo() -> Run {
        let mem: Arc<dyn Source> = Arc::new(demo_source(0));
        let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), mem];
        Run::new(sources, [(1, VPath::root()), (1, VPath::new("/docs"))], "")
    }

    #[test]
    fn end_scrolls_to_the_last_row() {
        let fs = vfs::MemFs::new("many");
        for i in 0..300 {
            fs.add_file(&format!("/f{i:03}"), "");
        }
        let mut r = Run::new(vec![Arc::new(fs)], [(0, VPath::root()), (0, VPath::root())], "");
        r.keys("end");
        let t = r.text();
        let rows: Vec<&str> = t.lines().filter(|l| l.starts_with('f') && l.len() == 4).collect();
        assert!(rows.contains(&"f299"), "{rows:?}");
    }

    #[test]
    fn refresh_button_rereads_the_active_panel() {
        let mem: Arc<dyn Source> = Arc::new(demo_source(0));
        let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), mem.clone()];
        let mut r = Run::new(sources, [(1, VPath::root()), (1, VPath::new("/docs"))], "");
        mem.mkdir(&VPath::new("/fresh")).unwrap();
        assert!(!r.text().contains("[fresh]"));
        r.click_key("tb-refresh", MouseButton::Left);
        assert!(r.text().contains("[fresh]"), "{}", r.text());
    }

    #[test]
    fn properties_rename_and_chmod() {
        let mem: Arc<dyn Source> = Arc::new(demo_source(0));
        let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), mem.clone()];
        let mut r = Run::new(sources, [(1, VPath::root()), (1, VPath::new("/docs"))], "");
        let before = mem.stat(&VPath::new("/readme.txt")).unwrap().mode.unwrap();
        r.keys("end alt+enter");
        let shown = |r: &Run, n: u32| r.text().lines().any(|l| l == format!("{n:03o}"));
        assert!(shown(&r, before & 0o777), "{}", r.text());
        r.click_key("dlg-perm-ox", MouseButton::Left).click_key("dlg-perm-uw", MouseButton::Left);
        // The number follows the ticks.
        assert!(shown(&r, (before | 0o001) & !0o200 & 0o777), "{}", r.text());
        // A tick takes the focus: back to the name.
        r.click_key("dlg-name", MouseButton::Left).keys("cmd+a").typed("notes.txt");
        r.keys("enter");
        assert!(mem.stat(&VPath::new("/readme.txt")).is_err(), "renamed");
        let after = mem.stat(&VPath::new("/notes.txt")).unwrap().mode.unwrap();
        assert_eq!(after, (before | 0o001) & !0o200, "{before:o} → {after:o}");
        let t = r.text();
        assert!(t.contains("notes") && !t.contains("Properties"), "{t}");
        // A bad name: nothing changes, the dialog says why.
        r.keys("alt+enter cmd+a").typed("a/b");
        r.keys("enter");
        assert!(r.text().contains("Cannot rename"), "{}", r.text());
        assert!(mem.stat(&VPath::new("/notes.txt")).is_ok());
    }

    /// The whole way through the UI to a real storage; skipped unless `UISON_S3_ENDPOINT` is set (see vfs-s3 tests).
    #[cfg(feature = "s3")]
    #[test]
    fn add_s3_and_copy_into_it() {
        let Ok(endpoint) = std::env::var("UISON_S3_ENDPOINT") else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let s3 = vfs_s3::S3Fs::new(vfs_s3::Config {
            endpoint: endpoint.clone(),
            region: String::new(),
            access_key: var("UISON_S3_KEY"),
            secret_key: var("UISON_S3_SECRET"),
            session_token: None,
            ..vfs_s3::Config::default()
        });
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        let bucket = VPath::root().child(&format!("uison-ui-{t}"));
        s3.mkdir(&bucket).unwrap();
        let mut r = demo();
        r.dir = var("UISON_SHOTS");
        // The name typed is what the panel calls it.
        r.keys("ctrl+f").click_text("S3-compatible storage", MouseButton::Left).typed("Test MinIO");
        r.click_key("dlg-p1", MouseButton::Left).typed(&endpoint).shot("s3_dialog");
        r.click_key("dlg-p3", MouseButton::Left).typed(&var("UISON_S3_KEY"));
        r.click_key("dlg-s4", MouseButton::Left).typed(&var("UISON_S3_SECRET")).shot("s3_keys");
        r.keys("enter");
        assert!(r.text().contains("Test MinIO: /"), "{}", r.text());
        assert!(r.text().contains(&format!("[{}]", bucket.name())), "{}", r.text());
        r.shot("s3_buckets");
        // In the list of sources S3 is one row, not a row per bucket.
        r.keys("alt+f1");
        let t = r.text();
        assert!(t.contains("All buckets") && !t.lines().any(|l| l == bucket.name()), "{t}");
        r.keys("escape");
        // Into the bucket, then F5 of notes.md from the other panel.
        r.click_text(&format!("[{}]", bucket.name()), MouseButton::Left).keys("enter tab end up f5").shot("s3_copy_dialog");
        r.keys("enter tab").shot("s3_copied");
        assert!(s3.stat(&bucket.child("notes.md")).is_ok_and(|e| e.size == Some(21)));
        assert!(r.text().contains("notes"), "{}", r.text());
        s3.remove(&bucket.child("notes.md")).unwrap();
        s3.remove(&bucket).unwrap();
    }

    /// WebDAV through the UI: Connect → WebDAV, the form, the panel on the server, F5 onto it; the connection
    /// is then in Edit… of the menu. Skipped unless `UISON_WEBDAV_URL` is set (see vfs-webdav tests).
    #[cfg(feature = "webdav")]
    #[test]
    fn add_webdav_and_copy_onto_it() {
        let Ok(url) = std::env::var("UISON_WEBDAV_URL") else { return vfs::conformance::skip("UISON_WEBDAV_URL") };
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let cfg = vfs_webdav::Config { url: url.clone(), user: var("UISON_WEBDAV_USER"), password: var("UISON_WEBDAV_PASSWORD") };
        let dav = vfs_webdav::DavFs::connect(cfg).unwrap();
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        let dir = VPath::root().child(&format!("uison-ui-{t}"));
        dav.mkdir(&dir).unwrap();
        let mut r = demo();
        r.dir = var("UISON_SHOTS");
        r.keys("ctrl+f").click_text("WebDAV", MouseButton::Left).typed(&url);
        r.click_key("dlg-p1", MouseButton::Left).typed(&var("UISON_WEBDAV_USER"));
        r.click_key("dlg-s2", MouseButton::Left).typed(&var("UISON_WEBDAV_PASSWORD")).shot("webdav_form");
        r.keys("enter");
        let title = dav.info().title;
        assert!(r.text().contains(&format!("{title}: /")), "{}", r.text());
        assert!(r.text().contains(&format!("[{}]", dir.name())), "{}", r.text());
        r.click_text(&format!("[{}]", dir.name()), MouseButton::Left).keys("enter tab end up f5").keys("enter tab").shot("webdav_copied");
        assert!(dav.stat(&dir.child("notes.md")).is_ok_and(|e| e.size == Some(21)));
        // Remembered: in Edit… of the Connect menu.
        r.keys("ctrl+f").click_text("Edit…", MouseButton::Left);
        assert!(r.text().contains("Connections") && r.text().contains(&title), "{}", r.text());
        r.keys("escape");
        dav.remove(&dir.child("notes.md")).unwrap();
        dav.remove(&dir).unwrap();
    }

    /// SSH through the UI: connect, the note about a new host, F5 onto the server. Skipped unless
    /// `UISON_SFTP_SERVER` and `UISON_KNOWN_HOSTS` are set (see vfs-sftp tests).
    #[cfg(feature = "sftp")]
    #[test]
    fn add_sftp_and_copy_onto_the_server() {
        let (Ok(server), Ok(known)) = (std::env::var("UISON_SFTP_SERVER"), std::env::var("UISON_KNOWN_HOSTS")) else {
            return vfs::conformance::skip("UISON_SFTP_SERVER or UISON_KNOWN_HOSTS");
        };
        let password = std::env::var("UISON_SFTP_PASSWORD").unwrap_or_default();
        let (user, host, port) = vfs_sftp::parse_server(&server).unwrap();
        let cfg = vfs_sftp::Config {
            host,
            port,
            user,
            password: Some(password.clone()),
            known_hosts: known.into(),
            trust: vfs_sftp::Trust::Any,
            ..Default::default()
        };
        let (ssh, _) = vfs_sftp::SftpFs::connect(cfg).unwrap();
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        let dir = ssh.home().child(&format!("uison-ui-{t}"));
        ssh.mkdir(&dir).unwrap();
        let mut r = demo();
        r.dir = std::env::var("UISON_SHOTS").unwrap_or_default();
        r.keys("ctrl+f").click_text("SFTP", MouseButton::Left).typed(&server);
        r.click_key("dlg-s1", MouseButton::Left).typed(&password).shot("sftp_dialog");
        // Test: the form says it works and stays; nothing is opened or remembered.
        r.click_text("Test", MouseButton::Left).wait(Duration::from_millis(1500));
        r.settle();
        assert!(r.text().contains("It works: connected to") && r.text().contains("New SFTP connection"), "{}", r.text());
        r.shot("sftp_tested");
        r.keys("enter");
        assert!(r.text().contains(&format!("sftp:{server}: {}", ssh.home())), "{}", r.text());
        r.shot("sftp_home");
        r.click_text(&format!("[{}]", dir.name()), MouseButton::Left).keys("enter tab end up f5 enter tab").shot("sftp_copied");
        assert!(ssh.stat(&dir.child("notes.md")).is_ok_and(|e| e.size == Some(21)));
        ssh.remove(&dir.child("notes.md")).unwrap();
        ssh.remove(&dir).unwrap();
    }

    /// The first server of `UISON_FTP_SERVERS` (see vfs-ftp tests) with this scheme: `host:port`.
    #[cfg(feature = "ftp")]
    fn ftp_server(scheme: &str) -> Option<String> {
        let all = std::env::var("UISON_FTP_SERVERS").ok()?;
        all.split_whitespace().find_map(|u| u.strip_prefix(&format!("{scheme}://")).map(str::to_string))
    }

    /// FTP through the UI: Connect → FTP / FTPS, plain FTP chosen, the panel at the user's home, F5 onto it.
    #[cfg(feature = "ftp")]
    #[test]
    fn add_ftp_and_copy_onto_it() {
        let Some(server) = ftp_server("ftp") else { return vfs::conformance::skip("UISON_FTP_SERVERS (an ftp:// one)") };
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let (_, host, port) = vfs_ftp::split_server(&server).unwrap();
        let cfg = vfs_ftp::Config {
            host,
            port: port.unwrap_or(0),
            user: var("UISON_FTP_USER"),
            password: var("UISON_FTP_PASSWORD"),
            security: vfs_ftp::Security::None,
            trusted: Vec::new(),
        };
        let ftp = vfs_ftp::FtpFs::connect(cfg).unwrap();
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
        let dir = ftp.home().child(&format!("uison-ui-{t}"));
        ftp.mkdir(&dir).unwrap();
        let mut r = demo();
        r.dir = var("UISON_SHOTS");
        r.keys("ctrl+f").click_text("FTP / FTPS", MouseButton::Left).typed(&server);
        r.click_key("dlg-p1", MouseButton::Left).typed(&var("UISON_FTP_USER"));
        r.click_key("dlg-s2", MouseButton::Left).typed(&var("UISON_FTP_PASSWORD"));
        r.click_text("None: plain FTP, the password goes in the clear", MouseButton::Left).shot("ftp_form");
        r.click_text("Connect", MouseButton::Left).wait(Duration::from_millis(1500));
        r.settle();
        let title = ftp.info().title;
        assert!(r.text().contains(&format!("{title}: {}", ftp.home())), "{}", r.text());
        r.click_text(&format!("[{}]", dir.name()), MouseButton::Left).keys("enter tab end up f5").keys("enter tab").shot("ftp_copied");
        assert!(ftp.stat(&dir.child("notes.md")).is_ok_and(|e| e.size == Some(21)));
        ftp.remove(&dir.child("notes.md")).unwrap();
        ftp.remove(&dir).unwrap();
    }

    /// FTPS to a server whose certificate the system doesn't trust: the panel doesn't open until the
    /// fingerprint is trusted in a dialog; then it is remembered and not asked again.
    #[cfg(feature = "ftp")]
    #[test]
    fn an_untrusted_certificate_is_asked_about() {
        let Some(server) = ftp_server("ftpes") else { return vfs::conformance::skip("UISON_FTP_SERVERS (an ftpes:// one)") };
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        let known = std::env::temp_dir().join(format!("uc-known-certs-{}", std::process::id()));
        // Only the FTP checks read it.
        unsafe { std::env::set_var("UISON_KNOWN_CERTS", &known) };
        let mut r = demo();
        r.dir = var("UISON_SHOTS");
        r.keys("ctrl+f").click_text("FTP / FTPS", MouseButton::Left).typed(&server);
        r.click_key("dlg-p1", MouseButton::Left).typed(&var("UISON_FTP_USER"));
        r.click_key("dlg-s2", MouseButton::Left).typed(&var("UISON_FTP_PASSWORD")).keys("enter");
        r.wait(Duration::from_millis(1500));
        r.settle();
        let t = r.text();
        assert!(t.contains("A certificate this Mac does not trust") && t.contains("SHA256:"), "{t}");
        assert!(!t.contains("ftps:"), "not open before the yes: {t}");
        r.shot("ftps_certificate");
        r.click_text("Trust and connect", MouseButton::Left).wait(Duration::from_millis(1500));
        r.settle();
        let t = r.text();
        let (_, host, port) = vfs_ftp::split_server(&server).unwrap();
        let port = port.map(|p| format!(":{p}")).unwrap_or_default();
        assert!(t.contains(&format!("ftps:{host}{port}: /")), "{t}");
        assert!(t.contains("its certificate (SHA256:"), "the note: {t}");
        // Remembered: the next connection asks nothing.
        let c = crate::connect::all().into_iter().find(|c| c.id == "ftp").unwrap();
        let args = vfs::ActionArgs(vec![
            ("server".into(), server.clone()),
            ("user".into(), var("UISON_FTP_USER")),
            ("password".into(), var("UISON_FTP_PASSWORD")),
        ]);
        let again = (c.connect)(&args);
        let local = vfs_local::LocalFs::new();
        let mut saved = String::new();
        std::io::Read::read_to_string(&mut local.open_read(&vfs_local::vpath(&known), 0).unwrap(), &mut saved).unwrap();
        let _ = local.remove(&vfs_local::vpath(&known));
        assert!(matches!(again, Ok((_, None))), "connected without asking");
        assert!(saved.starts_with(&format!("{host}:")) && saved.contains(" SHA256:"), "{saved}");
    }

    /// A source action with parameters: the source fills them in (in a worker), then the dialog asks them.
    #[test]
    fn action_parameters_come_from_the_source() {
        let mut r = demo();
        r.keys("end alt+enter");
        r.click_text("Copy link…", MouseButton::Left);
        let t = r.text();
        assert!(t.contains("Link type") && t.contains("mem://"), "{t}");
    }

    #[test]
    fn keys_after_a_copy() {
        let mut r = demo();
        r.keys("down down insert down insert f5 enter");
        let t = r.text();
        assert!(t.contains("3 files, 2 folders ·") && !t.contains("Selected"), "the selection is gone after the copy: {t}");
        r.keys("tab");
        let t = r.text();
        assert!(t.contains("mem:demo: /docs"), "{t}");
    }

    /// Two panels on one memory source, optionally slow.
    fn on(mem: &Arc<vfs::MemFs>, left: &str, right: &str) -> Run {
        let m: Arc<dyn Source> = mem.clone();
        Run::new(vec![Arc::new(vfs_local::LocalFs::new()), m], [(1, VPath::new(left)), (1, VPath::new(right))], "")
    }

    /// Picking a source whose folder can't be read: the panel stays on the old source and folder (it once
    /// switched the source alone, and F8 would have deleted the old path on the new one).
    #[test]
    fn a_failed_switch_keeps_the_panel() {
        let broken = Arc::new(vfs::MemFs::new("broken"));
        broken.fail_on("/");
        let broken2 = broken.clone();
        let mem: Arc<dyn Source> = Arc::new(demo_source(0));
        let sources: Vec<Arc<dyn Source>> = vec![Arc::new(vfs_local::LocalFs::new()), mem, broken];
        let mut r = Run::new(sources, [(1, VPath::new("/docs")), (1, VPath::root())], "");
        r.keys("alt+f1");
        r.click_text_after("mem:broken", "/", MouseButton::Left);
        let t = r.text();
        // The panel's title is still the old one (the error strip names the source it couldn't open).
        assert!(
            t.contains("simulated failure") && t.contains("Retry") && t.contains("mem:demo: /docs") && !t.lines().any(|l| l == "mem:broken: /"),
            "{t}"
        );
        // Retry after the source is healed: the panel goes there; the strip is gone.
        broken2.heal();
        r.click_key("left-retry", MouseButton::Left);
        r.settle();
        let t = r.text();
        assert!(t.contains("mem:broken: /") && !t.contains("Retry"), "{t}");
    }

    /// A question of an operation running in the background comes over the connection form being filled
    /// in: the question is never covered (it once was, and the operation waited for an answer forever);
    /// the connection's answer waits in the form, and the form comes back as it was.
    #[cfg(feature = "sftp")]
    #[test]
    fn a_question_is_never_covered() {
        let mem = Arc::new(demo_source(0).slow(1200));
        mem.add_file("/docs/notes.md", "other");
        mem.add_file("/notes.md", "mine");
        let mut r = on(&mem, "/", "/docs");
        // A "server" that takes the connection and says nothing until we close it.
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            let (conn, _) = l.accept().unwrap();
            let _ = rx.recv();
            drop(conn);
        });
        // F5 of notes.md onto /docs/notes.md, into the background before the question comes.
        r.click_text("notes", MouseButton::Left);
        r.h.event(Event::Key(key("f5"))).unwrap();
        r.wait(Duration::from_millis(500));
        r.h.event(Event::Key(key("enter"))).unwrap();
        r.wait(Duration::from_millis(100));
        r.click_key("job-bg", MouseButton::Left);
        r.keys("ctrl+f").click_text("SFTP", MouseButton::Left).typed(&format!("u@127.0.0.1:{port}"));
        r.click_key("dlg-s1", MouseButton::Left).typed("pw");
        assert!(!r.text().contains("The file already exists"), "too early: {}", r.text());
        r.keys("enter");
        assert!(r.text().contains("Connecting…"), "{}", r.text());
        r.wait_for("The file already exists", Duration::from_secs(10));
        tx.send(()).unwrap();
        server.join().unwrap();
        r.wait(Duration::from_millis(500));
        r.settle();
        let t = r.text();
        assert!(t.contains("The file already exists") && !t.contains("New SFTP connection"), "the question stays: {t}");
        r.click_text("Skip", MouseButton::Left);
        r.wait(Duration::from_millis(1000));
        r.settle();
        // The form again, with what was typed and why it didn't connect.
        let t = r.text();
        assert!(t.contains("New SFTP connection") && t.contains(&format!("u@127.0.0.1:{port}")) && !t.contains("Connecting…"), "{t}");
        assert!(t.to_lowercase().contains("connection"), "why: {t}");
        assert_eq!(mem.read("/docs/notes.md").unwrap(), b"other");
    }

    /// A connection that fails says why in its form, which stays with what was typed; Test too.
    #[cfg(feature = "sftp")]
    #[test]
    fn a_failed_connection_stays_in_its_form() {
        let mut r = demo();
        r.keys("ctrl+f");
        assert!(r.text().contains("SFTP") && r.text().contains("Edit…"), "the menu: {}", r.text());
        r.click_text("SFTP", MouseButton::Left).typed("u@127.0.0.1:1");
        r.click_key("dlg-s1", MouseButton::Left).typed("pw").keys("enter");
        // Windows refuses a closed port on 127.0.0.1 only after its SYN retries, about 2 s.
        r.wait_gone("Connecting…", Duration::from_secs(5));
        let t = r.text();
        assert!(t.contains("New SFTP connection") && t.contains("u@127.0.0.1:1") && t.contains("127.0.0.1"), "{t}");
        assert!(t.to_lowercase().contains("refused") && !t.contains("Connecting…"), "why, in the form: {t}");
        assert!(!t.contains("Cannot connect"), "no other window: {t}");
        r.click_text("Test", MouseButton::Left);
        assert!(r.text().contains("Checking the connection…") || r.text().contains("refused"), "{}", r.text());
        r.wait_gone("Checking", Duration::from_secs(5));
        assert!(r.text().contains("refused") && !r.text().contains("Checking"), "{}", r.text());
        r.keys("escape");
        assert!(!r.text().contains("New SFTP connection"), "{}", r.text());
    }

    /// Connect is a menu: the kinds, then Edit… — the connections added, with Add (the kinds again), Edit and
    /// Delete of the chosen one, Close.
    #[test]
    fn the_connect_menu_and_the_connections() {
        let mut r = demo();
        r.click_key("tb-connect", MouseButton::Left);
        let t = r.text();
        let kinds: Vec<&str> = crate::connect::all().iter().map(|c| c.label).collect();
        assert!(kinds.iter().all(|k| t.contains(k)) && t.contains("Edit…"), "{t}");
        r.click_text("Edit…", MouseButton::Left);
        let t = r.text();
        assert!(t.contains("Connections") && t.contains("Nothing here yet") && t.contains("Add ▾") && t.contains("Close"), "{t}");
        assert!(!t.contains("~/.ssh/config"), "only what was added: {t}");
        // Add: the kinds without Edit…; one opens its form.
        if let Some(c) = crate::connect::all().into_iter().next() {
            r.click_key("conn-add", MouseButton::Left);
            assert!(r.text().contains(c.label), "{}", r.text());
            r.click_text(c.label, MouseButton::Left);
            assert!(r.text().contains(&format!("New {} connection", c.label)), "{}", r.text());
            r.keys("escape");
        }
        r.keys("ctrl+f").click_text("Edit…", MouseButton::Left).click_key("conn-close", MouseButton::Left);
        assert!(!r.text().contains("Connections"), "{}", r.text());
    }

    /// The gear: the theme (the symbols' ink with it) and the language, taken at once; the panel stays until
    /// Escape or a click beside it.
    #[test]
    fn settings_theme_and_language() {
        fn ink(items: &[uison::Item], t: &str) -> Option<uison::Color> {
            items.iter().find_map(|it| match &it.shape {
                uison::Shape::Text { text, color, .. } if text.as_ref() == t => Some(*color),
                uison::Shape::Clip { items, .. } => ink(items, t),
                _ => None,
            })
        }
        fn images(items: &[uison::Item], out: &mut String) {
            for it in items {
                match &it.shape {
                    uison::Shape::Image { src, .. } => out.push_str(src),
                    uison::Shape::Clip { items, .. } => images(items, out),
                    _ => {}
                }
            }
        }
        let pics = |r: &Run| {
            let mut s = String::new();
            images(&r.h.rt.frame.items, &mut s);
            s
        };
        let mut r = demo();
        let light = ink(&r.h.rt.frame.items, "readme").unwrap();
        assert!(!r.text().contains("Language"), "closed at the start");
        r.click_key("tb-settings", MouseButton::Left);
        let t = r.text();
        assert!(t.contains("Theme") && t.contains("Language") && t.contains("English") && t.contains("Русский"), "{t}");
        r.click_key("set-dark", MouseButton::Left);
        assert_ne!(ink(&r.h.rt.frame.items, "readme").unwrap(), light, "the dark theme's ink");
        // Symbols are drawn on a Mac and on Windows (`icons::symbol`).
        assert!(
            !cfg!(any(target_os = "macos", windows)) || pics(&r).contains("sym-arrow.clockwise-dark"),
            "the symbols in the dark ink: {}",
            pics(&r)
        );
        assert!(r.text().contains("Language"), "a choice keeps the panel open");
        r.click_key("set-ru", MouseButton::Left);
        let t = r.text();
        assert!(t.contains("Язык") && t.contains("Тема") && t.contains("Копировать") && t.contains("Справка"), "{t}");
        assert!(t.contains("Имя") && t.contains("Размер"), "the panels' columns too: {t}");
        r.keys("escape");
        assert!(!r.text().contains("Язык"), "Escape closes it");
        // A click beside it closes it too; the theme of the system again, and English.
        r.click_key("tb-settings", MouseButton::Left).click_key("set-system", MouseButton::Left).click_key("set-en", MouseButton::Left);
        assert_eq!(ink(&r.h.rt.frame.items, "readme").unwrap(), light);
        assert!(r.text().contains("Language") && r.text().contains("Copy"), "{}", r.text());
        r.click(600.0, 400.0, MouseButton::Left);
        assert!(!r.text().contains("Language"), "{}", r.text());
    }

    /// The screens of `--png` in Russian: every text the app asks for has its translation.
    #[test]
    fn every_text_has_its_russian() {
        crate::i18n::set(crate::i18n::Lang::Ru);
        png_in("", false);
        let missing = crate::i18n::missing();
        assert!(missing.is_empty(), "not in the Russian table:\n{}", missing.join("\n"));
    }

    /// Help is a menu: the keys, and About with the version and the website.
    #[test]
    fn help_menu_and_about() {
        let mut r = demo();
        r.click_key("tb-help", MouseButton::Left);
        assert!(r.text().contains("About uison-commander") && r.text().contains("Keys"), "{}", r.text());
        r.click_text("About uison-commander", MouseButton::Left);
        let t = r.text();
        let site = crate::HOMEPAGE.trim_start_matches("https://");
        assert!(t.contains(&format!("Version {}", env!("CARGO_PKG_VERSION"))) && t.contains(site), "{t}");
        assert!(!t.contains("Open the website"), "{t}");
        r.keys("escape");
        assert!(!r.text().contains(site), "{}", r.text());
        r.click_key("tb-help", MouseButton::Left).click_text("Keys", MouseButton::Left);
        assert!(r.text().contains("Back to the previous folder"), "{}", r.text());
    }

    /// Alt+← and Alt+→ walk the folders a panel was in; Shift+F4 makes an empty file.
    #[test]
    fn history_and_a_new_file() {
        let mem = Arc::new(demo_source(0));
        let mut r = on(&mem, "/", "/photos");
        let left = |r: &Run| r.text().lines().rev().nth(1).unwrap_or_default().to_string();
        r.click_text("[docs]", MouseButton::Left).keys("enter");
        r.click_text("[archive]", MouseButton::Left).keys("enter");
        assert_eq!(left(&r), "mem:demo: /docs/archive");
        r.keys("alt+left");
        assert_eq!(left(&r), "mem:demo: /docs");
        r.keys("alt+left");
        assert_eq!(left(&r), "mem:demo: /");
        r.click_key("tb-forward", MouseButton::Left);
        assert_eq!(left(&r), "mem:demo: /docs");
        r.keys("shift+f4").typed("new.txt").keys("enter");
        assert_eq!(mem.read("/docs/new.txt").unwrap(), b"");
        assert!(r.text().contains("new"), "{}", r.text());
        // Going somewhere new drops the way forward.
        r.keys("alt+left alt+right backspace");
        assert_eq!(left(&r), "mem:demo: /");
        r.keys("alt+right");
        assert_eq!(left(&r), "mem:demo: /", "nothing forward");
    }

    /// Cancel in "Rename…" of a conflict goes back to the question; a bad name asks again.
    #[test]
    fn rename_in_a_conflict_comes_back() {
        let mem = Arc::new(demo_source(0));
        mem.add_file("/notes.md", "mine");
        let mut r = on(&mem, "/", "/docs");
        r.click_text("notes", MouseButton::Left).keys("f5 enter");
        r.click_text("Rename…", MouseButton::Left).keys("escape");
        assert!(r.text().contains("The file already exists"), "{}", r.text());
        r.click_text("Rename…", MouseButton::Left).typed("a/b").keys("enter");
        assert!(r.text().contains("The file already exists"), "{}", r.text());
        r.click_text("Rename…", MouseButton::Left).typed("mine.md").keys("enter");
        assert_eq!(mem.read("/docs/mine.md").unwrap(), b"mine");
    }

    /// Shift+F6 renames in this panel, whatever the path looks like; with several objects it offers a mask.
    #[test]
    fn shift_f6_stays_in_this_panel() {
        let mem = Arc::new(demo_source(0));
        let mut r = on(&mem, "/docs", "/");
        r.click_text("notes", MouseButton::Left).keys("shift+f6 cmd+a").typed("sub/new.md").keys("enter");
        assert!(mem.exists("/docs/sub/new.md") && !mem.exists("/sub/new.md"), "{:?}", mem.paths());
        r.keys("home down insert insert shift+f6");
        assert!(r.text().contains("*.*"), "{}", r.text());
    }

    /// Ctrl+U while both panels still read: both come, nothing stays "Loading…"; Escape stops a slow read.
    #[test]
    fn swap_and_escape_while_reading() {
        let mem = Arc::new(demo_source(400));
        let mut r = on(&mem, "/", "/docs");
        r.keys("ctrl+r tab ctrl+r ctrl+u");
        r.settle();
        let t = r.text();
        assert!(!t.contains("Loading…") && t.contains("mem:demo: /docs") && t.contains("[archive]"), "{t}");
        // Into a folder, and Escape before it is read: the panel stays.
        r.keys("home down enter escape");
        r.settle();
        assert!(r.text().contains("mem:demo: /docs"), "{}", r.text());
    }

    /// A remembered SFTP connection (by key: no secret) brings its panel back at startup where it was; one
    /// that needs a password is listed in Alt+F1 and Delete forgets it. Needs the test sshd and its key.
    #[cfg(feature = "sftp")]
    #[test]
    fn remembered_connections() {
        let (Ok(server), Ok(key), Ok(_)) = (std::env::var("UISON_SFTP_SERVER"), std::env::var("UISON_SFTP_KEY"), std::env::var("UISON_KNOWN_HOSTS"))
        else {
            return vfs::conformance::skip("UISON_SFTP_SERVER, UISON_SFTP_KEY or UISON_KNOWN_HOSTS");
        };
        let (user, host, port) = vfs_sftp::parse_server(&server).unwrap();
        let id = format!("sftp://{user}@{host}{}", if port == 22 { String::new() } else { format!(":{port}") });
        // Files through LocalFs: `std::fs` stays in vfs-local even in checks.
        let local = vfs_local::LocalFs::new();
        let dir = vfs_local::vpath(&std::env::temp_dir()).child(&format!("uc-config-{}", std::process::id()));
        let _ = local.mkdir(&dir);
        let write = |name: &str, text: &str| {
            let mut w = local.create(&dir.child(name), &vfs::CreateOpts { overwrite: true, ..Default::default() }).unwrap();
            std::io::Write::write_all(&mut w, text.as_bytes()).unwrap();
            w.finish().unwrap();
        };
        let profiles = [
            crate::connect::Profile {
                kind: "sftp".into(),
                id: id.clone(),
                title: id.replace("sftp://", "sftp:"),
                args: vec![("server".into(), server.clone()), ("key".into(), key)],
                secrets: vec![],
            },
            crate::connect::Profile {
                kind: "sftp".into(),
                id: "sftp://nobody@example.invalid".into(),
                title: "sftp:nobody@example.invalid".into(),
                args: vec![("server".into(), "nobody@example.invalid".into())],
                secrets: vec!["password".into()],
            },
        ];
        write("connections.json", &crate::connect::to_json(&profiles));
        let state = format!(
            "{{ \"left\": {{ \"src\": \"{id}\", \"dir\": \"/config\", \"sort\": \"name\", \"desc\": false, \"hidden\": true }}, \"right\": {{ \"src\": \"local\", \"dir\": \"/\", \"sort\": \"name\", \"desc\": false, \"hidden\": true }}, \"active\": 1 }}"
        );
        write("state.json", &state);
        // Only this test reads the variable (the others start `Fixed`).
        unsafe { std::env::set_var("UISON_CONFIG_DIR", crate::os_path(&dir)) };
        let mut r = Run::saved(vec![Arc::new(vfs_local::LocalFs::new())]);
        r.wait(Duration::from_millis(1500));
        r.settle();
        let t = r.text();
        assert!(t.contains(&format!("{}: /config", id.replace("sftp://", "sftp:"))), "the panel is back on the server: {t}");
        // The other one waits in Alt+F1; Delete forgets it.
        r.keys("alt+f2");
        assert!(r.text().contains("sftp:nobody@example.invalid"), "{}", r.text());
        assert!(!r.text().contains("~/.ssh/config"), "only the connections added, not the hosts of ~/.ssh/config: {}", r.text());
        r.keys("end delete");
        assert!(!r.text().contains("sftp:nobody@example.invalid"), "{}", r.text());
        r.keys("escape").wait(Duration::from_millis(300));
        let mut saved = String::new();
        std::io::Read::read_to_string(&mut local.open_read(&dir.child("connections.json"), 0).unwrap(), &mut saved).unwrap();
        assert!(saved.contains(&id) && !saved.contains("nobody"), "{saved}");
        // Edit… of the Connect menu: only what was added (no hosts of ~/.ssh/config).
        r.keys("ctrl+f").click_text("Edit…", MouseButton::Left);
        let t = r.text();
        assert!(t.contains("Connections") && t.contains(&id.replace("sftp://", "sftp:")) && !t.contains("~/.ssh/config"), "{t}");
        r.click_key("conn-close", MouseButton::Left);
        assert!(!r.text().contains("Connections"), "{}", r.text());
        unsafe { std::env::remove_var("UISON_CONFIG_DIR") };
        for f in ["connections.json", "state.json"] {
            let _ = local.remove(&dir.child(f));
        }
        let _ = local.remove(&dir);
    }

    /// Alt+F1 opens at once; a slow source says "reading…" on its own line until its roots come.
    #[test]
    fn source_list_does_not_wait_for_a_slow_source() {
        let slow: Arc<dyn Source> = Arc::new(demo_source(1500));
        let mut r = Run::new(vec![Arc::new(vfs_local::LocalFs::new()), slow], [(0, VPath::root()), (0, VPath::root())], "");
        let t = std::time::Instant::now();
        r.h.event(Event::Key(key("alt+f1"))).unwrap();
        let _ = r.h.event(Event::Wake);
        let text = r.text();
        assert!(t.elapsed() < Duration::from_millis(1000), "{:?}", t.elapsed());
        assert!(text.contains("reading…") && text.contains(DISK), "{text}");
        r.wait(Duration::from_millis(2000));
        r.settle();
        assert!(!r.text().contains("reading…"), "{}", r.text());
        // One click on a row opens it.
        r.click_text_after("mem:demo", "/", MouseButton::Left);
        r.wait(Duration::from_millis(1700));
        r.settle();
        assert!(r.text().contains("mem:demo: /") && !r.text().contains(DISK), "{}", r.text());
    }

    /// A server never seen: the panel doesn't open until its fingerprint is trusted in a dialog. Through the
    /// test proxy (`127.0.0.1:2223`, a host name known_hosts doesn't have), whose line is removed first.
    #[cfg(feature = "sftp")]
    #[test]
    fn a_new_server_is_trusted_by_its_fingerprint() {
        let (Ok(_), Ok(known)) = (std::env::var("UISON_SFTP_SERVER"), std::env::var("UISON_KNOWN_HOSTS")) else {
            return vfs::conformance::skip("UISON_SFTP_SERVER or UISON_KNOWN_HOSTS");
        };
        if std::net::TcpStream::connect("127.0.0.1:2223").is_err() {
            return vfs::conformance::skip("the test proxy on 127.0.0.1:2223 (tools/test-env.sh)");
        }
        let password = std::env::var("UISON_SFTP_PASSWORD").unwrap_or_default();
        // Forget 127.0.0.1:2223 (other tests use other names).
        let local = vfs_local::LocalFs::new();
        let path = vfs_local::vpath(std::path::Path::new(&known));
        let mut text = String::new();
        if let Ok(mut r) = local.open_read(&path, 0) {
            std::io::Read::read_to_string(&mut r, &mut text).unwrap();
        }
        let kept: String = text.lines().filter(|l| !l.starts_with("[127.0.0.1]:2223")).map(|l| format!("{l}\n")).collect();
        let mut w = local.create(&path, &vfs::CreateOpts { overwrite: true, ..Default::default() }).unwrap();
        std::io::Write::write_all(&mut w, kept.as_bytes()).unwrap();
        w.finish().unwrap();
        let mut r = demo();
        r.keys("ctrl+f").click_text("SFTP", MouseButton::Left).typed("uison@127.0.0.1:2223");
        r.click_key("dlg-s1", MouseButton::Left).typed(&password).keys("enter");
        r.wait(Duration::from_millis(1500));
        r.settle();
        let t = r.text();
        assert!(t.contains("A server never seen before") && t.contains("SHA256:"), "{t}");
        assert!(!t.contains("sftp:uison@127.0.0.1:2223: "), "not open before the yes: {t}");
        r.click_text("Trust and connect", MouseButton::Left);
        r.wait(Duration::from_millis(1500));
        r.settle();
        let t = r.text();
        assert!(t.contains("sftp:uison@127.0.0.1:2223: /config"), "{t}");
    }

    /// A notice in the corner says what happened and doesn't stand in the way of the mouse.
    #[test]
    fn a_notice_passes_clicks_through() {
        let mut r = demo();
        r.keys("end alt+enter");
        r.click_text("Copy link…", MouseButton::Left).keys("enter");
        assert!(r.text().contains("Copied to the clipboard"), "{}", r.text());
        // The click reaches the row under the notice's layer: Enter then opens that row, [docs].
        r.keys("home").click_key("left-r1", MouseButton::Left);
        assert!(r.text().contains("Copied to the clipboard"), "still there for a moment");
        r.keys("enter");
        assert!(r.text().contains("mem:demo: /docs"), "{}", r.text());
    }

    /// The panels are always half and half: as the window opens and after it is resized.
    #[test]
    fn the_panels_share_the_width_evenly() {
        let mut r = demo();
        let widths = |r: &Run| {
            let areas = uison::hit_areas(&r.h.rt.frame);
            let w = |k: &str| areas.iter().find(|(id, _)| *id == k).map(|(_, r)| r.w).unwrap_or_else(|| panic!("no '{k}'"));
            (w("left-hdr"), w("right-hdr"))
        };
        for size in [None, Some((1500.0, 700.0)), Some((760.0, 500.0))] {
            if let Some((w, h)) = size {
                r.h.event(Event::Resize(uison::Size::new(w, h))).unwrap();
                r.settle();
            }
            let (l, rt) = widths(&r);
            assert!(l > 200.0 && (l - rt).abs() < 1.0, "{size:?}: {l} vs {rt}");
        }
        // The panels have no edge to drag any more.
        assert!(!uison::hit_areas(&r.h.rt.frame).iter().any(|(id, _)| *id == "split"));
    }

    /// A column's edge in the title row: dragged left, the column grows in that panel only;
    /// a double click gives it its width back.
    #[test]
    fn columns_are_resized_by_their_edge() {
        let mut r = demo();
        let (size, date) = (r.x_of("left-col-size"), r.x_of("left-col-time"));
        let right = r.x_of("right-col-time") - r.x_of("right-col-size");
        r.drag_key("left-cw-size", -60.0);
        assert!((r.x_of("left-col-size") - (size - 60.0)).abs() < 2.0, "the size column starts 60 earlier");
        assert!((r.x_of("left-col-time") - date).abs() < 1.0, "the next one stays");
        assert!((r.x_of("right-col-time") - r.x_of("right-col-size") - right).abs() < 1.0, "the right panel keeps its widths");
        r.drag_key("right-cw-time", 30.0);
        assert!((r.x_of("left-col-size") - (size - 60.0)).abs() < 2.0, "and the left one keeps its own");
        r.click_key("left-cw-size", MouseButton::Left).click_key("left-cw-size", MouseButton::Left);
        assert!((r.x_of("left-col-size") - size).abs() < 2.0, "back to its width");
    }

    /// Sources and drives drop down under a panel's title: a click opens the list, one click on a row goes
    /// there; a click beside it or on the title again closes it.
    #[test]
    fn drives_drop_down_under_the_title() {
        let mut r = demo();
        r.click_key("right-hdr", MouseButton::Left);
        assert!(r.text().contains(DISK), "{}", r.text());
        r.click_key("right-hdr", MouseButton::Left);
        assert!(!r.text().contains(DISK), "the title again closes it: {}", r.text());
        r.click_key("left-hdr", MouseButton::Left).click_key("left-drop", MouseButton::Left);
        assert!(!r.text().contains(DISK), "a click beside it closes it: {}", r.text());
        // The first root of the disk: `/`, on Windows the drives (`vfs_local::roots`), shown as Windows writes them.
        let (root, shown) = if cfg!(windows) { ("C:", r"C:\") } else { ("/", "/") };
        r.click_key("left-hdr", MouseButton::Left).click_text_after(DISK, root, MouseButton::Left);
        assert!(r.text().lines().any(|l| l == format!("local: {shown}")), "{}", r.text());
        assert!(!r.text().contains(DISK), "{}", r.text());
    }

    /// A Host of ~/.ssh/config typed as the server: its HostName, Port, User and IdentityFile are used.
    #[cfg(feature = "sftp")]
    #[test]
    fn an_ssh_config_alias_connects() {
        let (Ok(_), Ok(key), Ok(_)) = (std::env::var("UISON_SFTP_SERVER"), std::env::var("UISON_SFTP_KEY"), std::env::var("UISON_KNOWN_HOSTS"))
        else {
            return vfs::conformance::skip("UISON_SFTP_SERVER, UISON_SFTP_KEY or UISON_KNOWN_HOSTS");
        };
        let local = vfs_local::LocalFs::new();
        let path = vfs_local::vpath(&std::env::temp_dir()).child(&format!("uc-ssh-config-{}", std::process::id()));
        let text = format!("Host testbox\n  HostName localhost\n  Port 2222\n  User uison\n  IdentityFile {key}\n");
        let mut w = local.create(&path, &vfs::CreateOpts { overwrite: true, ..Default::default() }).unwrap();
        std::io::Write::write_all(&mut w, text.as_bytes()).unwrap();
        w.finish().unwrap();
        // Only this test sets it; its only Host is `testbox`, so other connections are not changed by it.
        unsafe { std::env::set_var("UISON_SSH_CONFIG", crate::os_path(&path)) };
        let mut r = demo();
        r.keys("ctrl+f").click_text("SFTP", MouseButton::Left).typed("testbox").keys("enter");
        r.wait(Duration::from_millis(1500));
        r.settle();
        if r.text().contains("A server never seen before") {
            r.click_text("Trust and connect", MouseButton::Left).wait(Duration::from_millis(1500));
            r.settle();
        }
        let t = r.text();
        let _ = local.remove(&path);
        assert!(t.contains("sftp:uison@localhost:2222: /config"), "{t}");
    }

    /// A name in a panel's title opens that folder; the cursor lands on the folder we came out of.
    #[test]
    fn the_path_in_the_title_is_clickable() {
        let mem = Arc::new(demo_source(0));
        let mut r = on(&mem, "/", "/docs/archive");
        assert!(r.text().contains("mem:demo: /docs/archive"), "{}", r.text());
        r.click_key("right-cr1", MouseButton::Left);
        assert!(r.text().contains("mem:demo: /docs\n"), "{}", r.text());
        // The cursor is on [archive]: Enter goes back into it.
        r.keys("enter");
        assert!(r.text().contains("mem:demo: /docs/archive"), "{}", r.text());
        r.click_key("right-cr0", MouseButton::Left);
        assert!(r.text().lines().any(|l| l == "mem:demo: /"), "{}", r.text());
    }
}
