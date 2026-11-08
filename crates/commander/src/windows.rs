//! The bits of Win32 the program asks for (no crates): the system's Open dialog (`choose_file`: an SSH key for
//! a connection), the Credential Manager (`cred_*`: passwords of remembered connections), and icons as PNG
//! (`icon_png`: a file type's from the shell, `symbol_png`: a glyph of the system's icon font).

use std::ffi::c_void;
use std::path::Path;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// `OPENFILENAMEW`.
#[repr(C)]
struct OpenFileName {
    size: u32,
    owner: *mut c_void,
    instance: *mut c_void,
    filter: *const u16,
    custom_filter: *mut u16,
    max_custom_filter: u32,
    filter_index: u32,
    file: *mut u16,
    max_file: u32,
    file_title: *mut u16,
    max_file_title: u32,
    initial_dir: *const u16,
    title: *const u16,
    flags: u32,
    file_offset: u16,
    file_extension: u16,
    def_ext: *const u16,
    cust_data: isize,
    hook: *const c_void,
    template_name: *const u16,
    reserved: *mut c_void,
    reserved2: u32,
    flags_ex: u32,
}

#[link(name = "comdlg32")]
unsafe extern "system" {
    fn GetOpenFileNameW(ofn: *mut OpenFileName) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn GetActiveWindow() -> *mut c_void;
}

const OFN_HIDEREADONLY: u32 = 0x4;
const OFN_NOCHANGEDIR: u32 = 0x8;
const OFN_PATHMUSTEXIST: u32 = 0x800;
const OFN_FILEMUSTEXIST: u32 = 0x1000;
const OFN_EXPLORER: u32 = 0x8_0000;
const OFN_FORCESHOWHIDDEN: u32 = 0x1000_0000;

/// The system's Open dialog for one file, starting in `dir`, hidden files shown (keys live in ~/.ssh):
/// the chosen file's path, `None` if cancelled. Modal to the window: on the UI thread, it runs the messages itself.
pub(crate) fn choose_file(dir: &Path, title: &str) -> Option<String> {
    let mut file = vec![0u16; 32 * 1024];
    let dir = wide(&dir.to_string_lossy());
    let title = wide(title);
    let mut ofn = OpenFileName {
        size: size_of::<OpenFileName>() as u32,
        owner: unsafe { GetActiveWindow() },
        instance: std::ptr::null_mut(),
        filter: std::ptr::null(),
        custom_filter: std::ptr::null_mut(),
        max_custom_filter: 0,
        filter_index: 0,
        file: file.as_mut_ptr(),
        max_file: file.len() as u32,
        file_title: std::ptr::null_mut(),
        max_file_title: 0,
        initial_dir: dir.as_ptr(),
        title: title.as_ptr(),
        flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR | OFN_FORCESHOWHIDDEN,
        file_offset: 0,
        file_extension: 0,
        def_ext: std::ptr::null(),
        cust_data: 0,
        hook: std::ptr::null(),
        template_name: std::ptr::null(),
        reserved: std::ptr::null_mut(),
        reserved2: 0,
        flags_ex: 0,
    };
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let n = file.iter().position(|&c| c == 0).unwrap_or(file.len());
    Some(String::from_utf16_lossy(&file[..n]))
}

/// `CREDENTIALW`.
#[repr(C)]
struct Credential {
    flags: u32,
    kind: u32,
    target: *mut u16,
    comment: *mut u16,
    /// `FILETIME`: two halves, aligned as `u32`.
    written: [u32; 2],
    blob_size: u32,
    blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    alias: *mut u16,
    user: *mut u16,
}

#[link(name = "advapi32")]
unsafe extern "system" {
    fn CredWriteW(cred: *const Credential, flags: u32) -> i32;
    fn CredReadW(target: *const u16, kind: u32, flags: u32, out: *mut *mut Credential) -> i32;
    fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
    fn CredFree(buf: *mut c_void);
}

const CRED_TYPE_GENERIC: u32 = 1;
const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

/// A generic credential `target` holding `value` (as UTF-8) for this user on this computer.
pub(crate) fn cred_write(target: &str, user: &str, value: &str) -> bool {
    let mut t = wide(target);
    let mut u = wide(user);
    let mut v = value.as_bytes().to_vec();
    let c = Credential {
        flags: 0,
        kind: CRED_TYPE_GENERIC,
        target: t.as_mut_ptr(),
        comment: std::ptr::null_mut(),
        written: [0, 0],
        blob_size: v.len() as u32,
        blob: v.as_mut_ptr(),
        persist: CRED_PERSIST_LOCAL_MACHINE,
        attribute_count: 0,
        attributes: std::ptr::null_mut(),
        alias: std::ptr::null_mut(),
        user: u.as_mut_ptr(),
    };
    unsafe { CredWriteW(&c, 0) != 0 }
}

pub(crate) fn cred_read(target: &str) -> Option<String> {
    let t = wide(target);
    let mut c: *mut Credential = std::ptr::null_mut();
    if unsafe { CredReadW(t.as_ptr(), CRED_TYPE_GENERIC, 0, &mut c) } == 0 || c.is_null() {
        return None;
    }
    let out = unsafe {
        let c = &*c;
        let bytes = if c.blob.is_null() { &[][..] } else { std::slice::from_raw_parts(c.blob, c.blob_size as usize) };
        String::from_utf8(bytes.to_vec()).ok()
    };
    unsafe { CredFree(c.cast()) };
    out
}

pub(crate) fn cred_delete(target: &str) {
    let t = wide(target);
    unsafe { CredDeleteW(t.as_ptr(), CRED_TYPE_GENERIC, 0) };
}

/// `BITMAPINFO` with a 32-bit top-down header (no color table).
#[repr(C)]
struct BitmapInfo {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_ppm: i32,
    y_ppm: i32,
    clr_used: u32,
    clr_important: u32,
    colors: [u32; 1],
}

impl BitmapInfo {
    fn bgra(w: i32, h: i32) -> BitmapInfo {
        BitmapInfo {
            size: 40,
            width: w,
            height: -h,
            planes: 1,
            bit_count: 32,
            compression: 0,
            size_image: 0,
            x_ppm: 0,
            y_ppm: 0,
            clr_used: 0,
            clr_important: 0,
            colors: [0],
        }
    }
}

/// `SHFILEINFOW`.
#[repr(C)]
struct ShFileInfo {
    icon: *mut c_void,
    index: i32,
    attributes: u32,
    display_name: [u16; 260],
    type_name: [u16; 80],
}

/// `ICONINFO`.
#[repr(C)]
struct IconInfo {
    is_icon: i32,
    x_hotspot: u32,
    y_hotspot: u32,
    mask: *mut c_void,
    color: *mut c_void,
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: *mut c_void) -> *mut c_void;
    fn DeleteDC(dc: *mut c_void) -> i32;
    fn CreateDIBSection(dc: *mut c_void, bmi: *const BitmapInfo, usage: u32, bits: *mut *mut u8, section: *mut c_void, offset: u32) -> *mut c_void;
    fn SelectObject(dc: *mut c_void, obj: *mut c_void) -> *mut c_void;
    fn DeleteObject(obj: *mut c_void) -> i32;
    fn CreateFontW(
        h: i32,
        w: i32,
        esc: i32,
        orient: i32,
        weight: i32,
        italic: u32,
        underline: u32,
        strike: u32,
        charset: u32,
        out_precision: u32,
        clip_precision: u32,
        quality: u32,
        pitch: u32,
        face: *const u16,
    ) -> *mut c_void;
    fn GetTextFaceW(dc: *mut c_void, n: i32, face: *mut u16) -> i32;
    fn SetTextColor(dc: *mut c_void, color: u32) -> u32;
    fn SetBkMode(dc: *mut c_void, mode: i32) -> i32;
    fn GetDIBits(dc: *mut c_void, bm: *mut c_void, start: u32, lines: u32, bits: *mut u8, bmi: *mut BitmapInfo, usage: u32) -> i32;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn DrawTextW(dc: *mut c_void, text: *const u16, n: i32, rect: *mut [i32; 4], format: u32) -> i32;
    fn GetIconInfo(icon: *mut c_void, info: *mut IconInfo) -> i32;
    fn DestroyIcon(icon: *mut c_void) -> i32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn SHGetFileInfoW(path: *const u16, attrs: u32, info: *mut ShFileInfo, size: u32, flags: u32) -> usize;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
}

/// The icon of a kind (`/` — folder, `` — any file, otherwise an extension) as the shell shows it, 16 or 32
/// pixels, as PNG. Only the type is asked (`SHGFI_USEFILEATTRIBUTES`): no file is touched.
pub(crate) fn icon_png(kind: &str, px: u32) -> Option<Vec<u8>> {
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const SHGFI_SMALLICON: u32 = 0x1;
    const SHGFI_USEFILEATTRIBUTES: u32 = 0x10;
    const SHGFI_ICON: u32 = 0x100;
    let (name, attrs) = match kind {
        "/" => ("folder".to_string(), FILE_ATTRIBUTE_DIRECTORY),
        "" => ("file".to_string(), FILE_ATTRIBUTE_NORMAL),
        ext => (format!("file.{ext}"), FILE_ATTRIBUTE_NORMAL),
    };
    let size = if px <= 16 { SHGFI_SMALLICON } else { 0 };
    // The shell wants COM on the thread; already there (the window's) is fine too. 2: apartment-threaded.
    unsafe { CoInitializeEx(std::ptr::null_mut(), 2) };
    let mut info: ShFileInfo = unsafe { std::mem::zeroed() };
    let name = wide(&name);
    let flags = SHGFI_ICON | SHGFI_USEFILEATTRIBUTES | size;
    if unsafe { SHGetFileInfoW(name.as_ptr(), attrs, &mut info, size_of::<ShFileInfo>() as u32, flags) } == 0 || info.icon.is_null() {
        return None;
    }
    let out = unsafe { icon_rgba(info.icon) };
    unsafe { DestroyIcon(info.icon) };
    let (w, h, rgba) = out?;
    Some(png(w, h, &rgba))
}

/// A bitmap's pixels, `w`×`h`, as BGRA rows from the top.
unsafe fn bitmap_bgra(bm: *mut c_void, w: i32, h: i32) -> Option<Vec<u8>> {
    let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
    let mut bmi = BitmapInfo::bgra(w, h);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let n = unsafe { GetDIBits(dc, bm, 0, h as u32, px.as_mut_ptr(), &mut bmi, 0) };
    unsafe { DeleteDC(dc) };
    (n == h).then_some(px)
}

/// An icon's pixels as straight RGBA. Icons without alpha (old ones) take it from their mask.
unsafe fn icon_rgba(icon: *mut c_void) -> Option<(u32, u32, Vec<u8>)> {
    let mut ii: IconInfo = unsafe { std::mem::zeroed() };
    if unsafe { GetIconInfo(icon, &mut ii) } == 0 {
        return None;
    }
    let out = (|| {
        // A black-and-white icon (no color bitmap): too old to bother with.
        if ii.color.is_null() {
            return None;
        }
        // The size: GetDIBits with no bits fills in the header.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let mut bmi = BitmapInfo::bgra(0, 0);
        bmi.bit_count = 0;
        let ok = unsafe { GetDIBits(dc, ii.color, 0, 0, std::ptr::null_mut(), &mut bmi, 0) };
        unsafe { DeleteDC(dc) };
        let (w, h) = (bmi.width, bmi.height.abs());
        if ok == 0 || w <= 0 || h <= 0 {
            return None;
        }
        let mut px = unsafe { bitmap_bgra(ii.color, w, h) }?;
        if px.chunks(4).all(|p| p[3] == 0) {
            let mask = unsafe { bitmap_bgra(ii.mask, w, h) }?;
            for (p, m) in px.chunks_mut(4).zip(mask.chunks(4)) {
                p[3] = if m[0] == 0 { 255 } else { 0 };
            }
        }
        for p in px.chunks_mut(4) {
            p.swap(0, 2);
        }
        Some((w as u32, h as u32, px))
    })();
    unsafe {
        DeleteObject(ii.color);
        DeleteObject(ii.mask);
    }
    out
}

/// The glyph for an SF Symbol name in the system's icon font (Segoe Fluent Icons; on Windows 10 Segoe MDL2
/// Assets), `px` pixels square, in one color, as PNG; `None` — no such glyph or no such font.
pub(crate) fn symbol_png(name: &str, rgb: [f64; 3], px: i32) -> Option<Vec<u8>> {
    let glyph = match name {
        "chevron.left" => '\u{E76B}',
        "chevron.right" => '\u{E76C}',
        "arrow.clockwise" => '\u{E72C}',
        "network" => '\u{E968}',
        "gearshape" => '\u{E713}',
        "questionmark.circle" => '\u{E9CE}',
        "sun.max" => '\u{E706}',
        "moon" => '\u{E708}',
        "circle.lefthalf.filled" => '\u{E793}',
        "checkmark.circle" => '\u{E930}',
        "exclamationmark.triangle" => '\u{E7BA}',
        "hourglass" => '\u{E823}',
        "photo.on.rectangle" => '\u{E91B}',
        "internaldrive" => '\u{EDA2}',
        "cloud" => '\u{E753}',
        "server.rack" => '\u{E839}',
        "globe" => '\u{E774}',
        "memorychip" => '\u{E950}',
        "folder" => '\u{E8B7}',
        "eye" => '\u{E890}',
        "pencil" => '\u{E70F}',
        "doc.on.doc" => '\u{E8C8}',
        "arrow.right.doc.on.clipboard" => '\u{E8DE}',
        "folder.badge.plus" => '\u{E8F4}',
        "trash" => '\u{E74D}',
        "power" => '\u{E7E8}',
        _ => return None,
    };
    let (w, alpha) = glyph_alpha(&glyph.to_string(), px)?;
    let ink = rgb.map(|c| (c * 255.0).round() as u8);
    let rgba: Vec<u8> = alpha.iter().flat_map(|&a| [ink[0], ink[1], ink[2], a]).collect();
    Some(png(w as u32, px as u32, &rgba))
}

/// `text` in the icon font, centered in a row `h` pixels high (`h` wide per character): its width and the
/// coverage of each pixel (0–255).
fn glyph_alpha(text: &str, h: i32) -> Option<(i32, Vec<u8>)> {
    const ANTIALIASED_QUALITY: u32 = 4;
    const DT_CENTER: u32 = 0x1;
    const DT_VCENTER: u32 = 0x4;
    const DT_SINGLELINE: u32 = 0x20;
    const DT_NOPREFIX: u32 = 0x800;
    let t: Vec<u16> = text.encode_utf16().collect();
    let w = h * t.len() as i32;
    unsafe {
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        let mut bits: *mut u8 = std::ptr::null_mut();
        let bm = CreateDIBSection(dc, &BitmapInfo::bgra(w, h), 0, &mut bits, std::ptr::null_mut(), 0);
        if bm.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old_bm = SelectObject(dc, bm);
        // The first font that is there: for a name it doesn't have, GDI quietly gives another one.
        let mut font = std::ptr::null_mut();
        for face in ["Segoe Fluent Icons", "Segoe MDL2 Assets"] {
            let name = wide(face);
            let f = CreateFontW(-(h * 13 / 16), 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, ANTIALIASED_QUALITY, 0, name.as_ptr());
            let prev = SelectObject(dc, f);
            let mut got = [0u16; 64];
            let n = GetTextFaceW(dc, got.len() as i32, got.as_mut_ptr());
            SelectObject(dc, prev);
            if n > 1 && String::from_utf16_lossy(&got[..n as usize - 1]).eq_ignore_ascii_case(face) {
                font = f;
                break;
            }
            DeleteObject(f);
        }
        let out = (!font.is_null()).then(|| {
            let old_font = SelectObject(dc, font);
            SetTextColor(dc, 0x00FF_FFFF);
            // 1: TRANSPARENT.
            SetBkMode(dc, 1);
            let mut r = [0, 0, w, h];
            DrawTextW(dc, t.as_ptr(), t.len() as i32, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
            SelectObject(dc, old_font);
            DeleteObject(font);
            // White on black: any channel is the coverage.
            std::slice::from_raw_parts(bits, (w * h * 4) as usize).chunks(4).map(|p| p[1]).collect::<Vec<u8>>()
        });
        SelectObject(dc, old_bm);
        DeleteObject(bm);
        DeleteDC(dc);
        out.filter(|a| a.iter().any(|&v| v > 0)).map(|a| (w, a))
    }
}

/// Straight RGBA rows as PNG: zlib with stored (not compressed) blocks — icons are small.
fn png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    fn crc(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend((data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend(kind);
        out.extend(data);
        let c = crc(&out[start..]);
        out.extend(c.to_be_bytes());
    }
    // Each row: filter 0 (none), then its pixels.
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in rgba.chunks(w as usize * 4) {
        raw.push(0);
        raw.extend(row);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        z.extend((b.len() as u16).to_le_bytes());
        z.extend((!(b.len() as u16)).to_le_bytes());
        z.extend(*b);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    // 8 bits per channel, RGBA.
    ihdr.extend([8, 6, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn icons_render() {
        assert!(super::icon_png("/", 32).is_some_and(|p| p.starts_with(b"\x89PNG")), "the folder icon");
        assert!(super::icon_png("txt", 32).is_some(), "a file type's icon");
        assert!(super::symbol_png("gearshape", [0.0, 0.0, 0.0], 32).is_some(), "a symbol (an icon font is there)");
        assert!(super::symbol_png("no.such.symbol.anywhere", [0.0, 0.0, 0.0], 32).is_none());
    }
}
