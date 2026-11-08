//! The bits of AppKit and Foundation the program asks for, through the Objective-C runtime (no crates):
//! file type icons and SF Symbols as PNG (`icons`), Unicode composition of names (`nfc`), and the system's
//! Open dialog (`choose_file`: an SSH key for a connection), the light or dark look of the program
//! (`set_appearance`).

use std::ffi::{CString, c_char, c_void};

type Id = *mut c_void;
type Sel = *const c_void;

#[repr(C)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

unsafe fn class(name: &str) -> Id {
    let n = CString::new(name).unwrap();
    unsafe { objc_getClass(n.as_ptr()) }
}

unsafe fn sel(name: &str) -> Sel {
    let n = CString::new(name).unwrap();
    unsafe { sel_registerName(n.as_ptr()) }
}

/// `[obj sel]`
unsafe fn send0(obj: Id, s: &str) -> Id {
    let f: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, sel(s)) }
}

/// `[obj sel:a]`
unsafe fn send1(obj: Id, s: &str, a: *const c_void) -> Id {
    let f: unsafe extern "C" fn(Id, Sel, *const c_void) -> Id = unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { f(obj, sel(s), a) }
}

/// The icon of a file type (an extension or a UTI) drawn `px` pixels wide, as PNG.
pub(crate) unsafe fn icon_png(ty: &str, px: f64) -> Option<Vec<u8>> {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let out = (|| {
            let ty = CString::new(ty).ok()?;
            let s = send1(class("NSString"), "stringWithUTF8String:", ty.as_ptr().cast());
            let ws = send0(class("NSWorkspace"), "sharedWorkspace");
            let img = send1(ws, "iconForFileType:", s);
            if img.is_null() {
                return None;
            }
            to_png(img, px, px)
        })();
        objc_autoreleasePoolPop(pool);
        out
    }
}

/// An SF Symbol (`arrow.clockwise`) at `pt` points in one color, drawn at `scale`× as PNG; `None` if this
/// macOS doesn't have it.
pub(crate) unsafe fn symbol_png(name: &str, pt: f64, rgb: [f64; 3], scale: f64) -> Option<Vec<u8>> {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let out = (|| {
            let n = CString::new(name).ok()?;
            let s = send1(class("NSString"), "stringWithUTF8String:", n.as_ptr().cast());
            let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
            let img = f(class("NSImage"), sel("imageWithSystemSymbolName:accessibilityDescription:"), s, std::ptr::null_mut());
            if img.is_null() {
                return None;
            }
            // Size (regular weight) and color (a palette of one) of the symbol.
            let sized: unsafe extern "C" fn(Id, Sel, f64, f64) -> Id = std::mem::transmute(objc_msgSend as *const ());
            let size_cfg = sized(class("NSImageSymbolConfiguration"), sel("configurationWithPointSize:weight:"), pt, 0.0);
            let rgba: unsafe extern "C" fn(Id, Sel, f64, f64, f64, f64) -> Id = std::mem::transmute(objc_msgSend as *const ());
            let color = rgba(class("NSColor"), sel("colorWithSRGBRed:green:blue:alpha:"), rgb[0], rgb[1], rgb[2], 1.0);
            let colors = send1(class("NSArray"), "arrayWithObject:", color);
            let color_cfg = send1(class("NSImageSymbolConfiguration"), "configurationWithPaletteColors:", colors);
            if size_cfg.is_null() || color_cfg.is_null() {
                return None;
            }
            let cfg = send1(size_cfg, "configurationByApplyingConfiguration:", color_cfg);
            let img = send1(img, "imageWithSymbolConfiguration:", cfg);
            if img.is_null() {
                return None;
            }
            let size: unsafe extern "C" fn(Id, Sel) -> Size = std::mem::transmute(objc_msgSend as *const ());
            let sz = size(img, sel("size"));
            to_png(img, (sz.w * scale).ceil(), (sz.h * scale).ceil())
        })();
        objc_autoreleasePoolPop(pool);
        out
    }
}

#[repr(C)]
struct Size {
    w: f64,
    h: f64,
}

/// An NSImage drawn at `w`×`h` pixels, as PNG.
unsafe fn to_png(img: Id, w: f64, h: f64) -> Option<Vec<u8>> {
    unsafe {
        // The representation that fits best, as a CGImage.
        let rect = Rect { x: 0.0, y: 0.0, w, h };
        let f: unsafe extern "C" fn(Id, Sel, *const Rect, Id, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let cg = f(img, sel("CGImageForProposedRect:context:hints:"), &rect, std::ptr::null_mut(), std::ptr::null_mut());
        if cg.is_null() {
            return None;
        }
        let rep = send1(send0(class("NSBitmapImageRep"), "alloc"), "initWithCGImage:", cg);
        if rep.is_null() {
            return None;
        }
        // NSBitmapImageFileTypePNG = 4.
        let f: unsafe extern "C" fn(Id, Sel, usize, Id) -> Id = std::mem::transmute(objc_msgSend as *const ());
        let data = f(rep, sel("representationUsingType:properties:"), 4, send0(class("NSDictionary"), "dictionary"));
        let bytes = if data.is_null() {
            None
        } else {
            let len = send0(data, "length") as usize;
            let ptr = send0(data, "bytes") as *const u8;
            (!ptr.is_null() && len > 0).then(|| std::slice::from_raw_parts(ptr, len).to_vec())
        };
        send0(rep, "release");
        bytes
    }
}

/// A string in Unicode composed form (NFC): `и` + combining breve → `й`. HFS+ and SMB keep names
/// decomposed (NFD), what is typed comes composed; comparing the two needs one form.
pub(crate) fn nfc(s: &str) -> String {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let out = (|| {
            let c = CString::new(s).ok()?;
            let ns = send1(class("NSString"), "stringWithUTF8String:", c.as_ptr().cast());
            if ns.is_null() {
                return None;
            }
            let composed = send0(ns, "precomposedStringWithCanonicalMapping");
            let p = send0(composed, "UTF8String") as *const std::ffi::c_char;
            (!p.is_null()).then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        })();
        objc_autoreleasePoolPop(pool);
        out.unwrap_or_else(|| s.to_string())
    }
}

/// The system's Open dialog for one file, starting in `dir`, hidden files shown (keys live in ~/.ssh):
/// the chosen file's path, `None` if cancelled. Modal: on the UI thread, it runs the event loop itself.
pub(crate) fn choose_file(dir: &std::path::Path, message: &str) -> Option<String> {
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let out = (|| {
            let flag: unsafe extern "C" fn(Id, Sel, bool) = std::mem::transmute(objc_msgSend as *const ());
            let ns = |s: &str| -> Option<Id> {
                let c = CString::new(s).ok()?;
                Some(send1(class("NSString"), "stringWithUTF8String:", c.as_ptr().cast()))
            };
            let panel = send0(class("NSOpenPanel"), "openPanel");
            if panel.is_null() {
                return None;
            }
            flag(panel, sel("setCanChooseFiles:"), true);
            flag(panel, sel("setCanChooseDirectories:"), false);
            flag(panel, sel("setAllowsMultipleSelection:"), false);
            flag(panel, sel("setShowsHiddenFiles:"), true);
            send1(panel, "setMessage:", ns(message)?);
            let url = send1(class("NSURL"), "fileURLWithPath:", ns(&dir.to_string_lossy())?);
            send1(panel, "setDirectoryURL:", url);
            // NSModalResponseOK = 1.
            let run: unsafe extern "C" fn(Id, Sel) -> isize = std::mem::transmute(objc_msgSend as *const ());
            if run(panel, sel("runModal")) != 1 {
                return None;
            }
            let path = send0(send0(panel, "URL"), "path");
            let p = send0(path, "UTF8String") as *const std::ffi::c_char;
            (!p.is_null()).then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        })();
        objc_autoreleasePoolPop(pool);
        out
    }
}

/// The program's look: `light` (Aqua), `dark` (Dark Aqua) or the system's (anything else). The window's frame
/// and the system's menus follow, and the window says so to uison (its `@env.dark`). On the UI thread.
pub(crate) fn set_appearance(theme: &str) {
    let name = match theme {
        "light" => "NSAppearanceNameAqua",
        "dark" => "NSAppearanceNameDarkAqua",
        _ => "",
    };
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let app = send0(class("NSApplication"), "sharedApplication");
        let look = match CString::new(name) {
            Ok(c) if !name.is_empty() => {
                let s = send1(class("NSString"), "stringWithUTF8String:", c.as_ptr().cast());
                send1(class("NSAppearance"), "appearanceNamed:", s)
            }
            _ => std::ptr::null_mut(),
        };
        send1(app, "setAppearance:", look);
        objc_autoreleasePoolPop(pool);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_compose() {
        assert_eq!(super::nfc("\u{438}\u{306}"), "й");
        assert_eq!(super::nfc("e\u{301}t\u{e9}"), "\u{e9}t\u{e9}");
        assert_eq!(super::nfc("plain"), "plain");
    }
}
