//! What survives a restart: each panel's source, folder and sort, the active panel, the theme and the language.
//! `~/.config/uison-commander/state.json`, written through `LocalFs` like any other file.

use std::io::{Read, Write};

use vfs::{CreateOpts, Source, VPath};
use vfs_local::LocalFs;

use crate::app::App;
use crate::i18n::Lang;
use crate::panel::{Sort, SortKey};

pub struct PanelState {
    pub src: String,
    pub dir: VPath,
    pub sort: Sort,
    pub hidden: bool,
    /// Column widths (ext, size, date, attributes), if changed.
    pub cols: Option<[f64; 4]>,
}

pub struct State {
    pub panels: [PanelState; 2],
    pub active: usize,
    /// `system`, `light` or `dark`.
    pub theme: String,
    pub lang: Lang,
}

/// `~/.config/uison-commander` (on Windows `%APPDATA%\uison-commander`), or `UISON_CONFIG_DIR` (checks keep
/// the user's files out of it).
pub fn config_dir() -> Option<VPath> {
    if let Some(d) = std::env::var_os("UISON_CONFIG_DIR") {
        return Some(vfs_local::vpath(std::path::Path::new(&d)));
    }
    if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA")?;
        return Some(vfs_local::vpath(std::path::Path::new(&appdata)).child("uison-commander"));
    }
    let home = std::env::home_dir()?;
    Some(vfs_local::vpath(&home).join(".config/uison-commander"))
}

fn file() -> Option<VPath> {
    Some(config_dir()?.child("state.json"))
}

impl State {
    pub fn of(app: &App) -> State {
        let p = |i: usize| {
            let p = &app.panels[i];
            PanelState { src: p.src.info().id, dir: p.dir.clone(), sort: p.sort, hidden: p.show_hidden, cols: app.cols[i] }
        };
        State { panels: [p(0), p(1)], active: app.active, theme: app.theme.clone(), lang: crate::i18n::lang() }
    }

    pub fn to_json(&self) -> String {
        let mut s = String::from("{\n");
        for (name, p) in ["left", "right"].iter().zip(&self.panels) {
            s.push_str(&format!("  \"{name}\": {{ \"src\": "));
            uison::json::write_str(&mut s, &p.src);
            s.push_str(", \"dir\": ");
            uison::json::write_str(&mut s, p.dir.as_str());
            s.push_str(&format!(", \"sort\": \"{}\", \"desc\": {}, \"hidden\": {}", p.sort.key.name(), p.sort.desc, p.hidden));
            if let Some([a, b, c, d]) = p.cols {
                s.push_str(&format!(", \"cols\": [{a}, {b}, {c}, {d}]"));
            }
            s.push_str(" },\n");
        }
        s.push_str(&format!("  \"active\": {},\n", self.active));
        s.push_str(&format!("  \"theme\": \"{}\",\n  \"lang\": \"{}\"\n}}\n", self.theme, self.lang.code()));
        s
    }

    fn parse(src: &str) -> Option<State> {
        let v = uison::json::parse(src).ok()?;
        // Widths were once shared by both panels (a "cols" beside them): each panel starts from those.
        let shared = cols(v.get("cols"));
        let panel = |name: &str| -> Option<PanelState> {
            let p = v.get(name)?;
            let s = |k: &str| p.get(k).and_then(|x| x.as_str()).map(str::to_string);
            Some(PanelState {
                src: s("src")?,
                dir: VPath::new(&s("dir")?),
                sort: Sort {
                    key: s("sort").as_deref().and_then(SortKey::parse).unwrap_or(SortKey::Name),
                    desc: p.get("desc").and_then(|x| x.as_bool()).unwrap_or(false),
                },
                hidden: p.get("hidden").and_then(|x| x.as_bool()).unwrap_or(true),
                cols: cols(p.get("cols")).or(shared),
            })
        };
        let active = v.get("active").and_then(|x| x.as_f64()).unwrap_or(0.0) as usize;
        let theme = v.get("theme").and_then(|x| x.as_str()).filter(|t| ["light", "dark", "system"].contains(t)).unwrap_or("system");
        let lang = v.get("lang").and_then(|x| x.as_str()).and_then(Lang::parse).unwrap_or(Lang::En);
        Some(State { panels: [panel("left")?, panel("right")?], active: active.min(1), theme: theme.into(), lang })
    }
}

/// Four column widths, or none.
fn cols(x: Option<&uison::json::Value>) -> Option<[f64; 4]> {
    let w: Vec<f64> = x?.as_array()?.iter().filter_map(|x| x.as_f64()).collect();
    <[f64; 4]>::try_from(w).ok()
}

/// At startup, before the window: a missing or broken file is no state.
pub fn load() -> Option<State> {
    let path = file()?;
    let mut s = String::new();
    LocalFs.open_read(&path, 0).ok()?.read_to_string(&mut s).ok()?;
    State::parse(&s)
}

/// In a worker: the UI thread doesn't touch the disk.
pub fn save(json: String) {
    crate::jobs::spawn(move || save_now(&json));
}

pub fn save_now(json: &str) {
    let Some(path) = file() else { return };
    let fs = LocalFs;
    // The folders, each level if missing.
    let mut d = VPath::root();
    for c in path.parent().unwrap_or_else(VPath::root).components() {
        d = d.child(c);
        let _ = fs.mkdir(&d);
    }
    if let Ok(mut w) = fs.create(&path, &CreateOpts { overwrite: true, ..CreateOpts::default() })
        && w.write_all(json.as_bytes()).is_ok()
    {
        let _ = w.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let st = State {
            panels: [
                PanelState {
                    src: "local".into(),
                    dir: VPath::new("/Users/x/My \"docs\""),
                    sort: Sort { key: SortKey::Size, desc: true },
                    hidden: false,
                    cols: Some([60.0, 100.0, 120.0, 80.0]),
                },
                PanelState { src: "mem:demo".into(), dir: VPath::root(), sort: Sort { key: SortKey::Name, desc: false }, hidden: true, cols: None },
            ],
            active: 1,
            theme: "dark".into(),
            lang: Lang::Ru,
        };
        let back = State::parse(&st.to_json()).unwrap();
        assert_eq!((back.theme.as_str(), back.lang), ("dark", Lang::Ru));
        assert_eq!(back.panels[0].dir, st.panels[0].dir);
        assert_eq!(back.panels[0].sort, st.panels[0].sort);
        assert!(!back.panels[0].hidden);
        assert_eq!((back.panels[1].src.as_str(), back.active), ("mem:demo", 1));
        assert_eq!((back.panels[0].cols, back.panels[1].cols), (st.panels[0].cols, None));
    }

    /// A file of when both panels shared the widths: each panel gets them.
    #[test]
    fn shared_widths_go_to_both_panels() {
        let old = r#"{ "left": { "src": "local", "dir": "/" }, "right": { "src": "local", "dir": "/tmp" },
            "cols": [60, 100, 120, 80], "active": 0 }"#;
        let st = State::parse(old).unwrap();
        assert_eq!((st.panels[0].cols, st.panels[1].cols), (Some([60.0, 100.0, 120.0, 80.0]), Some([60.0, 100.0, 120.0, 80.0])));
        // From before the settings: the system's theme, English.
        assert_eq!((st.theme.as_str(), st.lang), ("system", Lang::En));
    }
}
