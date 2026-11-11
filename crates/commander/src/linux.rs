//! Linux: what the desktop gives a program through its commands — the text editor (F4) and the Open dialog.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// KDE Plasma: its own dialogs first (kdialog); GNOME and the rest — zenity.
fn kde() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|p| p.eq_ignore_ascii_case("KDE")))
}

/// Run a command quietly and wait for it; `None` if it is not installed.
fn run(cmd: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(cmd).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()
}

/// The system's Open dialog started in `dir`: the chosen file, or `None` when cancelled (or there is no dialog).
/// It blocks until closed: call it from a worker thread.
pub(crate) fn choose_file(dir: &Path, title: &str) -> Option<String> {
    // A trailing slash: zenity opens the folder itself instead of selecting it in its parent.
    let mut start = dir.to_string_lossy().into_owned();
    if !start.ends_with('/') {
        start.push('/');
    }
    let zenity = ("zenity", vec!["--file-selection", "--title", title, "--filename", &start]);
    let kdialog = ("kdialog", vec!["--title", title, "--getopenfilename", &start]);
    let order = if kde() { [kdialog, zenity] } else { [zenity, kdialog] };
    for (cmd, args) in order {
        let Some(out) = run(cmd, &args) else { continue };
        // Installed: its answer is final, a cancel included.
        let path = String::from_utf8_lossy(&out.stdout).trim_end_matches(['\r', '\n']).to_string();
        return (out.status.success() && !path.is_empty()).then_some(path);
    }
    None
}

/// The default application for plain text (like `open -t` on macOS): the editor opens even a `.log` or a
/// file without an extension, which `xdg-open` would give to another program or to nobody.
pub(crate) fn open_in_editor(path: &Path) {
    let path: PathBuf = path.into();
    std::thread::spawn(move || {
        let desktop = run("xdg-mime", &["query", "default", "text/plain"])
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|d| !d.is_empty());
        let p = path.to_string_lossy();
        if let Some(d) = desktop {
            let app = d.trim_end_matches(".desktop");
            if run("gtk-launch", &[app, &p]).is_some_and(|o| o.status.success()) {
                return;
            }
        }
        // Waits for it: no zombie is left behind.
        let _ = run("xdg-open", &[&p]);
    });
}

// ---- Icons of the rows: the desktop's own icon theme (freedesktop.org Icon Theme and Shared MIME-info specs) ----

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// One folder of a theme: `32x32/mimetypes`, or `mimetypes/32` in Breeze.
struct Dir {
    path: String,
    size: i32,
    scale: i32,
    min: i32,
    max: i32,
    threshold: i32,
    /// `Scalable` or `Threshold` (a `Fixed` one fits only by its exact size).
    kind: String,
}

impl Dir {
    /// How far the folder's pictures are from `px` pixels (0 — they fit).
    fn distance(&self, px: i32) -> i32 {
        let s = self.scale;
        match self.kind.as_str() {
            "Scalable" => (self.min * s - px).max(px - self.max * s).max(0),
            "Threshold" => ((self.size - self.threshold) * s - px).max(px - (self.size + self.threshold) * s).max(0),
            _ => (self.size * s - px).abs(),
        }
    }
}

struct Theme {
    roots: Vec<PathBuf>,
    dirs: Vec<Dir>,
}

struct Icons {
    /// The theme, then what it inherits, then the fallbacks.
    themes: Vec<Theme>,
    /// Extension (lower case) → MIME type, by weight.
    globs: HashMap<String, String>,
    /// MIME type → the generic icon name the MIME database gives it.
    generic: HashMap<String, String>,
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::home_dir().unwrap_or_default();
    let xdg = std::env::var("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|_| home.join(".local/share"));
    let mut out = vec![xdg];
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    out.extend(dirs.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    out.extend(["/usr/local/share", "/usr/share"].map(PathBuf::from));
    let mut seen = HashSet::new();
    out.retain(|d| seen.insert(d.clone()));
    out
}

/// The name of the theme the user has chosen: Plasma's own setting on KDE, GNOME's elsewhere.
fn theme_name() -> Option<String> {
    let plasma = || {
        let home = std::env::home_dir()?;
        let text = vfs_local::read_system_text(&home.join(".config/kdeglobals"))?;
        let icons = text.split("\n[").find(|s| s.trim_start_matches('[').starts_with("Icons]"))?;
        icons.lines().find_map(|l| l.strip_prefix("Theme=")).map(|t| t.trim().to_string())
    };
    let gsettings = || {
        let out = run("gsettings", &["get", "org.gnome.desktop.interface", "icon-theme"])?;
        let name = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string();
        (out.status.success() && !name.is_empty()).then_some(name)
    };
    if kde() { plasma().or_else(gsettings) } else { gsettings().or_else(plasma) }
}

/// `index.theme` of a theme: what it inherits and its folders.
fn load_theme(name: &str, bases: &[PathBuf]) -> Option<(Theme, Vec<String>)> {
    let roots: Vec<PathBuf> = bases.iter().map(|b| b.join("icons").join(name)).filter(|r| r.is_dir()).collect();
    let index = roots.iter().find_map(|r| vfs_local::read_system_text(&r.join("index.theme")))?;
    let mut sections: HashMap<&str, HashMap<&str, &str>> = HashMap::new();
    let mut cur = "";
    for line in index.lines() {
        let line = line.trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            cur = s;
        } else if let Some((k, v)) = line.split_once('=') {
            sections.entry(cur).or_default().insert(k.trim(), v.trim());
        }
    }
    let head = sections.get("Icon Theme")?;
    let list = |k: &str| head.get(k).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>());
    let inherits = list("Inherits").unwrap_or_default();
    let dirs = list("Directories").unwrap_or_default().into_iter().chain(list("ScaledDirectories").unwrap_or_default());
    let dirs = dirs
        .filter_map(|path| {
            let s = sections.get(path.as_str())?;
            let num = |k: &str| s.get(k).and_then(|v| v.parse::<i32>().ok());
            let size = num("Size")?;
            Some(Dir {
                size,
                scale: num("Scale").unwrap_or(1).max(1),
                min: num("MinSize").unwrap_or(size),
                max: num("MaxSize").unwrap_or(size),
                threshold: num("Threshold").unwrap_or(2),
                kind: s.get("Type").copied().unwrap_or("Threshold").to_string(),
                path,
            })
        })
        .collect();
    Some((Theme { roots, dirs }, inherits))
}

fn load_icons() -> Icons {
    let bases = data_dirs();
    // The user's theme, what it inherits (a theme may be named by several), then Adwaita, Yaru and hicolor.
    let mut queue: Vec<String> = theme_name().into_iter().collect();
    queue.extend(["Yaru", "Adwaita", "hicolor"].map(String::from));
    let (mut themes, mut seen, mut i) = (Vec::new(), HashSet::new(), 0);
    while i < queue.len() {
        let name = queue[i].clone();
        i += 1;
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some((theme, inherits)) = load_theme(&name, &bases) {
            themes.push(theme);
            // Right after the theme itself: its parents come before the fallbacks.
            let at = i;
            for (k, p) in inherits.into_iter().enumerate() {
                queue.insert(at + k, p);
            }
        }
    }
    let (mut globs, mut weights): (HashMap<String, String>, HashMap<String, u32>) = Default::default();
    let mut generic = HashMap::new();
    // The user's own folder comes first: a type it defines wins at equal weight.
    for dir in &bases {
        if let Some(text) = vfs_local::read_system_text(&dir.join("mime/globs2")) {
            for line in text.lines().filter(|l| !l.starts_with('#')) {
                let mut f = line.split(':');
                let (Some(w), Some(mime), Some(glob)) = (f.next(), f.next(), f.next()) else { continue };
                // "*.ext" only: the pattern of an extension, not of a whole name.
                let Some(ext) = glob.strip_prefix("*.").filter(|e| !e.is_empty() && !e.contains(['*', '?', '[', '.'])) else { continue };
                let (ext, w) = (ext.to_ascii_lowercase(), w.parse::<u32>().unwrap_or(50));
                if weights.get(&ext).is_none_or(|&old| w > old) {
                    weights.insert(ext.clone(), w);
                    globs.insert(ext, mime.to_string());
                }
            }
        }
        if let Some(text) = vfs_local::read_system_text(&dir.join("mime/generic-icons")) {
            for (m, g) in text.lines().filter_map(|l| l.split_once(':')) {
                generic.entry(m.to_string()).or_insert_with(|| g.to_string());
            }
        }
    }
    Icons { themes, globs, generic }
}

impl Icons {
    /// The file of the icon `name` that fits `px` pixels best: the first theme that has it.
    fn find(&self, name: &str, px: i32) -> Option<PathBuf> {
        for theme in &self.themes {
            let mut best: Option<(i32, bool, PathBuf)> = None;
            for d in &theme.dirs {
                let dist = d.distance(px);
                for root in &theme.roots {
                    for (ext, svg) in [("png", false), ("svg", true)] {
                        let f = root.join(&d.path).join(format!("{name}.{ext}"));
                        // A picture of the right size, else the closest (a PNG before an SVG at the same distance).
                        if best.as_ref().is_none_or(|b| (dist, svg) < (b.0, b.1)) && f.is_file() {
                            best = Some((dist, svg, f));
                        }
                    }
                }
            }
            if let Some((_, _, f)) = best {
                return Some(f);
            }
        }
        None
    }

    /// Icon names to try for a kind (`/` — a folder, `` — an unknown file, otherwise an extension), best first.
    fn names(&self, kind: &str) -> Vec<String> {
        match kind {
            "/" => vec!["folder".into()],
            "" => vec!["application-octet-stream".into(), "text-x-generic".into()],
            ext => {
                let Some(mime) = self.globs.get(ext) else { return self.names("") };
                let mut out = vec![mime.replace('/', "-")];
                out.extend(self.generic.get(mime).cloned());
                if let Some((media, _)) = mime.split_once('/') {
                    out.push(format!("{media}-x-generic"));
                }
                out.extend(self.names(""));
                out
            }
        }
    }
}

fn icons() -> &'static Icons {
    static ICONS: OnceLock<Icons> = OnceLock::new();
    ICONS.get_or_init(load_icons)
}

/// The icon of a kind in the system's theme as PNG, `px` pixels.
pub(crate) fn icon_png(kind: &str, px: u32) -> Option<Vec<u8>> {
    let icons = icons();
    // uison draws PNG and JPEG; themes (Breeze, Adwaita) keep much of their icons as SVG: gdk-pixbuf turns them in.
    icons.names(kind).iter().find_map(|n| icons.find(n, px as i32).and_then(|f| pixbuf::to_png(&f, px as i32, None)))
}

/// Icon names of the desktop's themes for an SF Symbol name of the screen, best first.
fn symbol_names(name: &str) -> &'static [&'static str] {
    match name {
        "chevron.left" => &["go-previous"],
        "chevron.right" => &["go-next"],
        "arrow.clockwise" => &["view-refresh"],
        "network" => &["network-workgroup", "network-wired"],
        "gearshape" => &["preferences-system", "emblem-system", "configure"],
        "questionmark.circle" => &["help-about", "dialog-question", "help-contents"],
        "sun.max" => &["weather-clear", "display-brightness"],
        "moon" => &["weather-clear-night", "night-light"],
        "circle.lefthalf.filled" => &["contrast", "preferences-desktop-theme", "preferences-color", "color-management"],
        "checkmark.circle" => &["emblem-ok", "object-select", "dialog-ok"],
        "exclamationmark.triangle" => &["dialog-warning"],
        "hourglass" => &["content-loading", "preferences-system-time", "appointment-soon", "chronometer"],
        "photo.on.rectangle" => &["image-x-generic", "folder-pictures"],
        "internaldrive" => &["drive-harddisk"],
        "cloud" => &["weather-overcast", "weather-many-clouds", "network-server"],
        "server.rack" => &["network-server"],
        "globe" => &["web-browser", "applications-internet", "network-workgroup"],
        "memorychip" => &["media-memory", "drive-multidisk", "computer"],
        "folder" => &["folder"],
        "eye" => &["view-reveal", "document-properties", "view-visible"],
        "pencil" => &["document-edit", "edit"],
        "doc.on.doc" => &["edit-copy"],
        "arrow.right.doc.on.clipboard" => &["edit-paste", "go-jump"],
        "folder.badge.plus" => &["folder-new", "folder-add"],
        "trash" => &["user-trash", "edit-delete"],
        "power" => &["system-shutdown", "application-exit"],
        _ => &[],
    }
}

/// A button's symbol: the theme's symbolic icon (else its ordinary one) in one color, `px` pixels, as PNG.
pub(crate) fn symbol_png(name: &str, rgb: [f64; 3], px: i32) -> Option<Vec<u8>> {
    let icons = icons();
    let ink = rgb.map(|c| (c * 255.0).round() as u8);
    let find = |n: String| icons.find(&n, px);
    // The symbolic one of every name first: a colored icon of a theme turns into a flat silhouette.
    let file = symbol_names(name)
        .iter()
        .find_map(|n| find(format!("{n}-symbolic")))
        .or_else(|| symbol_names(name).iter().find_map(|n| find(n.to_string())))?;
    pixbuf::to_png(&file, px, Some(ink))
}

/// gdk-pixbuf by `dlopen`, like uison does: present on any desktop with GTK, and its SVG loader reads the themes.
mod pixbuf {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::path::Path;
    use std::sync::OnceLock;

    unsafe extern "C" {
        fn dlopen(file: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    const RTLD_NOW: c_int = 2;

    type FromFile = unsafe extern "C" fn(*const c_char, c_int, c_int, *mut *mut c_void) -> *mut c_void;
    type Save = unsafe extern "C" fn(*mut c_void, *mut *mut c_char, *mut usize, *const c_char, *mut *mut c_void, ...) -> c_int;
    type Free = unsafe extern "C" fn(*mut c_void);

    type Get = unsafe extern "C" fn(*const c_void) -> c_int;
    type Pixels = unsafe extern "C" fn(*const c_void) -> *mut u8;

    struct Api {
        width: Get,
        height: Get,
        stride: Get,
        channels: Get,
        pixels: Pixels,
        from_file: FromFile,
        save: Save,
        unref: Free,
        free: Free,
    }

    fn api() -> Option<&'static Api> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| unsafe {
            let open = |lib: &str| {
                let lib = CString::new(lib).ok()?;
                let h = dlopen(lib.as_ptr(), RTLD_NOW);
                (!h.is_null()).then_some(h)
            };
            let sym = |h: *mut c_void, name: &str| {
                let name = CString::new(name).ok()?;
                let p = dlsym(h, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            let (pb, go, gl) = (open("libgdk_pixbuf-2.0.so.0")?, open("libgobject-2.0.so.0")?, open("libglib-2.0.so.0")?);
            Some(Api {
                width: std::mem::transmute::<*mut c_void, Get>(sym(pb, "gdk_pixbuf_get_width")?),
                height: std::mem::transmute::<*mut c_void, Get>(sym(pb, "gdk_pixbuf_get_height")?),
                stride: std::mem::transmute::<*mut c_void, Get>(sym(pb, "gdk_pixbuf_get_rowstride")?),
                channels: std::mem::transmute::<*mut c_void, Get>(sym(pb, "gdk_pixbuf_get_n_channels")?),
                pixels: std::mem::transmute::<*mut c_void, Pixels>(sym(pb, "gdk_pixbuf_get_pixels")?),
                from_file: std::mem::transmute::<*mut c_void, FromFile>(sym(pb, "gdk_pixbuf_new_from_file_at_size")?),
                save: std::mem::transmute::<*mut c_void, Save>(sym(pb, "gdk_pixbuf_save_to_buffer")?),
                unref: std::mem::transmute::<*mut c_void, Free>(sym(go, "g_object_unref")?),
                free: std::mem::transmute::<*mut c_void, Free>(sym(gl, "g_free")?),
            })
        })
        .as_ref()
    }

    /// An image file of any kind gdk-pixbuf knows, scaled into `px`×`px` (the proportions stay), as PNG; with `ink`
    /// every pixel gets that color and keeps its transparency (a symbol of one color).
    pub fn to_png(file: &Path, px: i32, ink: Option<[u8; 3]>) -> Option<Vec<u8>> {
        let api = api()?;
        let path = CString::new(file.to_string_lossy().as_bytes()).ok()?;
        unsafe {
            let pb = (api.from_file)(path.as_ptr(), px, px, std::ptr::null_mut());
            if pb.is_null() {
                return None;
            }
            if let Some(ink) = ink {
                let (w, h, stride, n) = ((api.width)(pb) as usize, (api.height)(pb) as usize, (api.stride)(pb) as usize, (api.channels)(pb) as usize);
                let data = (api.pixels)(pb);
                // Only a picture with transparency can be a symbol; a flat one would become a square.
                if n != 4 || data.is_null() {
                    (api.unref)(pb);
                    return None;
                }
                for y in 0..h {
                    let row = std::slice::from_raw_parts_mut(data.add(y * stride), w * 4);
                    for p in row.as_chunks_mut::<4>().0 {
                        p[..3].copy_from_slice(&ink);
                    }
                }
            }
            let (mut buf, mut len): (*mut c_char, usize) = (std::ptr::null_mut(), 0);
            let ok = (api.save)(pb, &mut buf, &mut len, c"png".as_ptr(), std::ptr::null_mut(), std::ptr::null::<c_char>()) != 0;
            let png = (ok && !buf.is_null()).then(|| std::slice::from_raw_parts(buf.cast::<u8>(), len).to_vec());
            if !buf.is_null() {
                (api.free)(buf.cast());
            }
            (api.unref)(pb);
            png
        }
    }
}
