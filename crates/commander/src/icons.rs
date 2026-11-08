//! Icons of the rows: the system's own on a Mac (`NSWorkspace iconForFileType:`) — a folder, and a file
//! by its extension, the same as in Finder. Each kind is asked once and kept as a PNG image `mem:icon-…`.
//! Elsewhere there are no icons yet: the source is empty and nothing is drawn.

use std::cell::RefCell;
use std::collections::HashMap;

/// Side of the icon in pixels: 16 pt on a 2× screen.
#[cfg(target_os = "macos")]
const PX: f64 = 32.0;

/// Extensions longer than this, or not made of letters and digits, get the generic file icon:
/// a timestamp or a host name after the dot is not a type.
const MAX_EXT: usize = 8;

thread_local! {
    /// Kind → image source (empty: the system had none).
    static CACHE: RefCell<HashMap<String, String>> = RefCell::default();
}

/// The image source of a row's icon.
pub fn src(dir: bool, ext: &str) -> String {
    let kind = if dir {
        "/".to_string()
    } else if ext.len() <= MAX_EXT && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        ext.to_ascii_lowercase()
    } else {
        String::new()
    };
    CACHE.with(|c| {
        if let Some(s) = c.borrow().get(&kind) {
            return s.clone();
        }
        let s = match png(&kind) {
            Some(bytes) => {
                let name = match kind.as_str() {
                    "/" => "icon-dir".to_string(),
                    k => format!("icon-f-{k}"),
                };
                uison::image::put(&name, bytes);
                format!("mem:{name}")
            }
            None => String::new(),
        };
        c.borrow_mut().insert(kind, s.clone());
        s
    })
}

/// The icon of a kind (`/` — folder, `` — any file, otherwise an extension) as PNG.
#[cfg(target_os = "macos")]
fn png(kind: &str) -> Option<Vec<u8>> {
    let ty = match kind {
        "/" => "public.folder",
        "" => "public.data",
        ext => ext,
    };
    unsafe { crate::macos::icon_png(ty, PX) }
}

#[cfg(not(target_os = "macos"))]
fn png(_kind: &str) -> Option<Vec<u8>> {
    None
}

/// The two colors of toolbar symbols: on a light background and on a dark one.
pub const LIGHT_INK: [f64; 3] = [0.24, 0.24, 0.27];
pub const DARK_INK: [f64; 3] = [0.86, 0.86, 0.89];

/// An SF Symbol for a button, in the color for a light or a dark background, as an image source
/// (`mem:sym-…`); empty where there are no symbols (not a Mac).
pub fn symbol(name: &str, dark: bool) -> String {
    symbol_ink(name, if dark { "dark" } else { "light" }, if dark { DARK_INK } else { LIGHT_INK })
}

/// White: on the accent color (the active panel's title).
pub fn symbol_white(name: &str) -> String {
    symbol_ink(name, "white", [1.0, 1.0, 1.0])
}

fn symbol_ink(name: &str, ink: &str, rgb: [f64; 3]) -> String {
    let key = format!("sym-{name}-{ink}");
    CACHE.with(|c| {
        if let Some(s) = c.borrow().get(&key) {
            return s.clone();
        }
        #[cfg(target_os = "macos")]
        let bytes = unsafe { crate::macos::symbol_png(name, 13.0, rgb, 2.0) };
        #[cfg(not(target_os = "macos"))]
        let bytes: Option<Vec<u8>> = {
            let _ = rgb;
            None
        };
        let s = match bytes {
            Some(b) => {
                uison::image::put(&key, b);
                format!("mem:{key}")
            }
            None => String::new(),
        };
        c.borrow_mut().insert(key, s.clone());
        s
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::src;

    #[test]
    fn system_icons_by_kind() {
        let (dir, txt, pdf) = (src(true, "txt"), src(false, "txt"), src(false, "PDF"));
        assert_eq!((dir.as_str(), txt.as_str(), pdf.as_str()), ("mem:icon-dir", "mem:icon-f-txt", "mem:icon-f-pdf"));
        assert!(uison::image::natural_size(&txt).is_some_and(|s| s.w >= 16.0), "a PNG the backend can read");
        // Not a type: a timestamp after the dot.
        assert_eq!(src(false, "1770532892562"), "mem:icon-f-");
    }

    #[test]
    fn symbols_in_both_inks() {
        for name in ["arrow.clockwise", "externaldrive.connected.to.line.below", "eye", "trash"] {
            for dark in [false, true] {
                let s = super::symbol(name, dark);
                assert!(uison::image::natural_size(&s).is_some_and(|z| z.w >= 16.0), "{name} {dark}: {s}");
                if let Ok(dir) = std::env::var("UISON_SYMBOL_DUMP") {
                    let b = uison::image::bytes(&s).unwrap();
                    let p = vfs_local::vpath(std::path::Path::new(&dir)).child(&format!("{name}-{dark}.png"));
                    let mut w =
                        vfs::Source::create(&vfs_local::LocalFs::new(), &p, &vfs::CreateOpts { overwrite: true, ..Default::default() }).unwrap();
                    std::io::Write::write_all(&mut w, &b).unwrap();
                    w.finish().unwrap();
                }
            }
        }
        assert_eq!(super::symbol("no.such.symbol.anywhere", false), "");
    }
}
