//! The viewer (F3), like Lister in TC. It never reads a file whole: it keeps a window of a few hundred KB
//! around the first visible line and asks a worker for another window when the view leaves it. So a 2 GB
//! log opens at once, from any source.

use std::sync::Arc;

use vfs::{Source, VPath};

/// The window read at a time.
pub const WINDOW: u64 = 256 * 1024;
/// How far around the first visible line the window must reach.
const MARGIN: u64 = 64 * 1024;
/// A line longer than this is shown in pieces (a minified file, binary data in text mode).
const MAX_LINE: usize = 2048;
/// Images are shown whole up to this size.
pub const MAX_IMAGE: u64 = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Text,
    Hex,
}

pub struct Viewer {
    pub src: Arc<dyn Source>,
    pub path: VPath,
    pub size: u64,
    pub mode: Mode,
    pub wrap: bool,
    pub image: bool,
    /// Byte offset of the first visible line.
    pub top: u64,
    /// The loaded window: offset and bytes.
    pub cache: Option<(u64, Vec<u8>)>,
    /// A window being read (its offset).
    pub pending: Option<u64>,
    pub seq: u64,
    /// Go to the end once the last window is in.
    pub want_end: bool,
    pub rows: usize,
    pub cols: usize,
    pub search: String,
    /// The search running: a new one or closing the viewer stops it.
    pub searching: Option<vfs::Cancel>,
    pub found: Option<u64>,
    pub note: String,
}

impl Viewer {
    pub fn new(src: Arc<dyn Source>, path: VPath, size: u64, image: bool, seq: u64) -> Viewer {
        Viewer {
            src,
            path,
            size,
            mode: Mode::Text,
            wrap: false,
            image,
            top: 0,
            cache: None,
            pending: None,
            seq,
            want_end: false,
            rows: 40,
            cols: 120,
            search: String::new(),
            searching: None,
            found: None,
            note: String::new(),
        }
    }

    /// The window to read so that `top` has enough around it; `None` — the cache covers it.
    pub fn need(&self) -> Option<u64> {
        if self.image {
            return None;
        }
        let lo = self.top.saturating_sub(MARGIN);
        let hi = (self.top + MARGIN).min(self.size);
        let covered = self.cache.as_ref().is_some_and(|(off, b)| *off <= lo && off + b.len() as u64 >= hi);
        if covered || self.pending.is_some() {
            return None;
        }
        Some(self.top.saturating_sub(WINDOW / 2))
    }

    pub fn loaded(&mut self, offset: u64, bytes: Vec<u8>) {
        self.pending = None;
        self.cache = Some((offset, bytes));
        if self.want_end && offset + self.cache.as_ref().map_or(0, |c| c.1.len() as u64) >= self.size {
            self.want_end = false;
            self.end();
        }
    }

    fn byte(&self, pos: u64) -> Option<u8> {
        let (off, b) = self.cache.as_ref()?;
        pos.checked_sub(*off).and_then(|i| b.get(i as usize)).copied()
    }

    fn has(&self, pos: u64) -> bool {
        pos >= self.size || self.byte(pos).is_some()
    }

    /// Where the visual line starting at `pos` ends (the start of the next one).
    fn line_end(&self, pos: u64) -> u64 {
        if self.mode == Mode::Hex {
            return (pos + 16).min(self.size);
        }
        let limit = if self.wrap { self.cols.max(1) } else { MAX_LINE };
        let mut p = pos;
        let mut chars = 0;
        while p < self.size {
            let Some(b) = self.byte(p) else { break };
            if b == b'\n' {
                return p + 1;
            }
            // Count characters, not bytes: continuation bytes of UTF-8 don't start one.
            if b & 0xC0 != 0x80 {
                if chars == limit {
                    return p;
                }
                chars += 1;
            }
            p += 1;
        }
        p
    }

    /// The start of the visual line before `pos`.
    fn line_before(&self, pos: u64) -> u64 {
        if pos == 0 {
            return 0;
        }
        if self.mode == Mode::Hex {
            return pos.saturating_sub(16);
        }
        // The logical line that holds pos - 1: back to the newline before it.
        let mut start = pos - 1;
        let floor = pos.saturating_sub(MARGIN);
        while start > floor {
            match self.byte(start - 1) {
                Some(b'\n') | None => break,
                _ => start -= 1,
            }
        }
        // Then forward by visual lines up to pos.
        let mut cur = start;
        loop {
            let next = self.line_end(cur);
            if next >= pos || next == cur {
                return cur;
            }
            cur = next;
        }
    }

    pub fn down(&mut self, n: usize) {
        for _ in 0..n {
            let next = self.line_end(self.top);
            if next >= self.size || !self.has(next) {
                break;
            }
            self.top = next;
        }
    }

    pub fn up(&mut self, n: usize) {
        for _ in 0..n {
            if self.top == 0 || !self.has(self.top - 1) {
                break;
            }
            self.top = self.line_before(self.top);
        }
    }

    pub fn home(&mut self) {
        self.top = 0;
    }

    /// The last screen. In text mode the lines are only known near the end: read the last window first.
    pub fn end(&mut self) {
        if self.mode == Mode::Hex {
            let last = self.size.div_ceil(16).saturating_sub(self.rows as u64);
            self.top = last * 16;
            return;
        }
        if !self.has(self.size.saturating_sub(1)) {
            self.want_end = true;
            self.top = self.size.saturating_sub(WINDOW / 2);
            return;
        }
        // From the end of the file back by a screen: the last line ends the screen.
        self.top = self.size;
        self.up(self.rows.max(1));
    }

    pub fn set_mode(&mut self, m: Mode) {
        self.mode = m;
        // Keep the place: the start of a hex row, or of the text line holding it.
        self.top = match m {
            Mode::Hex => self.top / 16 * 16,
            Mode::Text => {
                let t = self.top;
                if t == 0 { 0 } else { self.line_before(t + 1) }
            }
        };
    }

    /// The visible lines.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.rows);
        let mut p = self.top;
        while out.len() < self.rows && p < self.size {
            if !self.has(p) {
                break;
            }
            let end = self.line_end(p);
            out.push(match self.mode {
                Mode::Hex => self.hex_line(p, end),
                Mode::Text => self.text_line(p, end),
            });
            if end == p {
                break;
            }
            p = end;
        }
        out
    }

    fn bytes(&self, a: u64, b: u64) -> Vec<u8> {
        (a..b).filter_map(|p| self.byte(p)).collect()
    }

    fn text_line(&self, a: u64, b: u64) -> String {
        let raw = self.bytes(a, b);
        let s = String::from_utf8_lossy(&raw);
        // Tabs as spaces, other control characters as dots: one line of text, as the frame needs.
        s.trim_end_matches(['\n', '\r'])
            .chars()
            .map(|c| match c {
                '\t' => "    ".to_string(),
                c if c.is_control() => "·".to_string(),
                c => c.to_string(),
            })
            .collect()
    }

    fn hex_line(&self, a: u64, b: u64) -> String {
        let raw = self.bytes(a, b);
        let mut s = format!("{a:010X}  ");
        for i in 0..16 {
            match raw.get(i) {
                Some(x) => s.push_str(&format!("{x:02X} ")),
                None => s.push_str("   "),
            }
            if i == 7 {
                s.push(' ');
            }
        }
        s.push(' ');
        s.extend(raw.iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' }));
        s
    }

    /// Position for the header: percent and offset.
    pub fn position(&self) -> String {
        let pct = if self.size == 0 { 100 } else { (self.top * 100 / self.size).min(100) };
        if self.image {
            let note = if self.note.is_empty() { String::new() } else { format!("{}  ·  ", self.note) };
            return format!("{}  ·  {note}{}", t!("picture"), crate::format::count(self.size, "byte"));
        }
        let mode = match self.mode {
            Mode::Text if self.wrap => t!("text, wrap"),
            Mode::Text => t!("text"),
            Mode::Hex => t!("hex"),
        };
        let note = if self.note.is_empty() { String::new() } else { format!("{}  ·  ", self.note) };
        format!("{note}{mode}  ·  {}  ·  {pct}%", crate::format::count(self.size, "byte"))
    }

    /// Jump to a found offset: the line that holds it goes to the top.
    pub fn show(&mut self, pos: u64) {
        self.found = Some(pos);
        self.top = match self.mode {
            Mode::Hex => pos / 16 * 16,
            Mode::Text => pos,
        };
        // In text mode the line start is known once the window around pos is in (`settle`).
    }

    /// After a jump: from the found byte back to its line start, when the window allows.
    pub fn settle(&mut self) {
        if self.mode == Mode::Text
            && let Some(f) = self.found
            && self.top == f
            && self.has(f)
            && f > 0
        {
            self.top = self.line_before(f + 1);
        }
    }
}

/// The character at `i` of UTF-8 bytes and its length; `None` in the middle of one or on a broken one.
fn char_at(b: &[u8], i: usize) -> Option<(char, usize)> {
    (1..=4).filter(|n| i + n <= b.len()).find_map(|n| std::str::from_utf8(&b[i..i + n]).ok().and_then(|s| s.chars().next().map(|c| (c, n))))
}

/// A character for comparing without case (simple folding: one character for one; `ß` stays `ß`).
fn fold(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

/// Search `needle` without case — any letters, Cyrillic too — from `from` forward, or backward before `from`.
/// Reads the file in windows through the source: works on any size and any source.
pub fn find(src: &dyn Source, path: &VPath, size: u64, needle: &[u8], from: u64, back: bool, cancel: &vfs::Cancel) -> vfs::Result<Option<u64>> {
    use std::io::Read;
    let want: Vec<char> = String::from_utf8_lossy(needle).chars().map(fold).collect();
    if want.is_empty() {
        return Ok(None);
    }
    // Windows overlap by enough bytes for the longest spelling of the needle (4 bytes a character).
    let n = (want.len() * 4) as u64;
    let chunk = 1u64 << 20;
    let matches_at = |buf: &[u8], i: usize| -> bool {
        let mut p = i;
        for w in &want {
            match char_at(buf, p) {
                Some((c, len)) if fold(c) == *w => p += len,
                _ => return false,
            }
        }
        true
    };
    // A match starts on a character, not inside one.
    let starts = |buf: &[u8], i: usize| buf[i] & 0xC0 != 0x80;
    let read = |off: u64, len: u64| -> vfs::Result<Vec<u8>> {
        let r = src.open_read(path, off)?;
        let mut buf = Vec::with_capacity(len as usize);
        r.take(len).read_to_end(&mut buf).map_err(|e| vfs::Error::io(path, &e))?;
        Ok(buf)
    };
    if !back {
        let mut off = from;
        while off < size {
            cancel.check()?;
            let buf = read(off, chunk + n)?;
            let last = buf.len().min(chunk as usize);
            if let Some(i) = (0..last).find(|&i| starts(&buf, i) && matches_at(&buf, i)) {
                return Ok(Some(off + i as u64));
            }
            off += chunk;
        }
    } else {
        let mut end = from.min(size);
        while end > 0 {
            cancel.check()?;
            let start = end.saturating_sub(chunk);
            let buf = read(start, (end - start + n).min(size - start))?;
            let last = (end - start) as usize;
            if let Some(i) = (0..last.min(buf.len())).rev().find(|&i| start + (i as u64) < from && starts(&buf, i) && matches_at(&buf, i)) {
                return Ok(Some(start + i as u64));
            }
            end = start;
        }
    }
    Ok(None)
}

pub fn is_image(name: &str) -> bool {
    matches!(vfs::split_ext(name).1.to_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp" | "tif" | "tiff" | "heic" | "webp")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::{Cancel, MemFs};

    fn viewer(text: &str) -> Viewer {
        let fs = MemFs::new("t");
        fs.add_file("/f", text);
        let mut v = Viewer::new(Arc::new(fs), VPath::new("/f"), text.len() as u64, false, 1);
        v.rows = 3;
        v.cols = 4;
        v.loaded(0, text.as_bytes().to_vec());
        v
    }

    #[test]
    fn text_lines_and_navigation() {
        let mut v = viewer("one\ntwo\nthree\nfour\nfive\n");
        assert_eq!(v.lines(), ["one", "two", "three"]);
        v.down(2);
        assert_eq!(v.lines(), ["three", "four", "five"]);
        v.down(10);
        assert_eq!(v.lines(), ["five"]);
        v.up(1);
        assert_eq!(v.lines()[0], "four");
        v.end();
        assert_eq!(v.lines(), ["three", "four", "five"]);
        v.home();
        v.wrap = true;
        v.down(2);
        assert_eq!(v.lines(), ["thre", "e", "four"], "wrapped by 4 characters");
        v.up(1);
        assert_eq!(v.lines()[0], "two");
    }

    #[test]
    fn hex_rows() {
        let mut v = viewer("0123456789abcdefXYZ");
        v.set_mode(Mode::Hex);
        let l = v.lines();
        assert_eq!(l.len(), 2);
        assert!(l[0].starts_with("0000000000  30 31") && l[0].ends_with("0123456789abcdef"), "{}", l[0]);
        assert!(l[1].ends_with("XYZ"));
    }

    #[test]
    fn search_both_ways() {
        let fs = MemFs::new("t");
        let mut data = vec![b'.'; (3 << 20) + 10];
        data[100..106].copy_from_slice(b"Needle");
        // Across the border of two 1 MB chunks.
        data[(1 << 20) - 2..(1 << 20) + 4].copy_from_slice(b"needle");
        fs.add_file("/big", data.clone());
        let (p, n, c) = (VPath::new("/big"), data.len() as u64, Cancel::new());
        assert_eq!(find(&fs, &p, n, b"NEEDLE", 0, false, &c).unwrap(), Some(100));
        assert_eq!(find(&fs, &p, n, b"needle", 101, false, &c).unwrap(), Some((1 << 20) - 2));
        assert_eq!(find(&fs, &p, n, b"needle", (1 << 20) - 1, false, &c).unwrap(), None);
        assert_eq!(find(&fs, &p, n, b"needle", n, true, &c).unwrap(), Some((1 << 20) - 2));
        assert_eq!(find(&fs, &p, n, b"needle", (1 << 20) - 2, true, &c).unwrap(), Some(100));
    }

    #[test]
    fn search_ignores_case_in_any_alphabet() {
        let fs = MemFs::new("t");
        let text = "Начало. ПРИВЕТ, мир. Ärger und Straße.";
        fs.add_file("/t", text);
        let (p, n, c) = (VPath::new("/t"), text.len() as u64, Cancel::new());
        let at = |s: &str| text.find(s).map(|i| i as u64);
        assert_eq!(find(&fs, &p, n, "привет".as_bytes(), 0, false, &c).unwrap(), at("ПРИВЕТ"));
        assert_eq!(find(&fs, &p, n, "МИР".as_bytes(), 0, false, &c).unwrap(), at("мир"));
        assert_eq!(find(&fs, &p, n, "ärger".as_bytes(), 0, false, &c).unwrap(), at("Ärger"));
        assert_eq!(find(&fs, &p, n, "начало".as_bytes(), n, true, &c).unwrap(), Some(0));
        assert_eq!(find(&fs, &p, n, "нет такого".as_bytes(), 0, false, &c).unwrap(), None);
    }
}
