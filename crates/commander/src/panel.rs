//! One panel: a source, a folder in it, the sorted listing, the cursor and the selection.
//! The panel only keeps state and writes it into the screen data; reading the folder is the app's job
//! (in a worker thread).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use uison::Data;
use uison::ui::Data as UiData;
use vfs::{Cancel, Entry, Kind, Source, VPath, split_ext};

use crate::format;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Ext,
    Time,
    Size,
}

impl SortKey {
    pub fn name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Ext => "ext",
            SortKey::Time => "time",
            SortKey::Size => "size",
        }
    }

    pub fn parse(s: &str) -> Option<SortKey> {
        [SortKey::Name, SortKey::Ext, SortKey::Time, SortKey::Size].into_iter().find(|k| k.name() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sort {
    pub key: SortKey,
    pub desc: bool,
}

/// One row of the panel in the screen data. Short names: a folder may have 50 000 of them.
#[derive(Data, Default)]
struct Row {
    /// Icon: an image source.
    i: String,
    /// Name without the extension (folders: the whole name, in brackets).
    n: String,
    e: String,
    s: String,
    d: String,
    a: String,
    dir: bool,
    sel: bool,
    hid: bool,
    /// The whole name, for the tooltip of a row cut short.
    t: String,
}

/// Extensions longer than this are shown as part of the name.
const MAX_EXT: usize = 6;

pub struct Item {
    pub entry: Entry,
    pub sel: bool,
}

pub struct Panel {
    /// `left` / `right`: the data path and the key prefix.
    pub side: &'static str,
    pub src: Arc<dyn Source>,
    pub dir: VPath,
    /// The listing as read, unsorted (sorting and hidden files don't re-read the folder).
    listed: Vec<Entry>,
    /// What is shown, sorted; row 0 is `..` unless at the root.
    pub items: Vec<Item>,
    /// Row index (with `..`).
    pub cursor: usize,
    pub sort: Sort,
    pub show_hidden: bool,
    /// The read in flight: its number (stale answers are dropped) and how to cancel it.
    pub seq: u64,
    pub loading: Option<Cancel>,
    pub error: Option<String>,
    /// What the failed read was (source and folder): Retry reads it again.
    pub failed: Option<(Arc<dyn Source>, VPath)>,
    /// Folder sizes counted with Space.
    pub sizes: HashMap<String, u64>,
    /// Free and total bytes of the volume (as of the last read), if the source knows.
    pub space: Option<(u64, u64)>,
    /// The counts running: leaving the folder stops them.
    pub sizing: Vec<Cancel>,
    /// Quick search text (typing letters), `None` — closed.
    pub quick: Option<String>,
    /// The folders left (Alt+←) and come back from (Alt+→), the last one last.
    pub back: Vec<(Arc<dyn Source>, VPath)>,
    pub forward: Vec<(Arc<dyn Source>, VPath)>,
    /// The read in flight is a step back (`Some(true)`) or forward through them.
    pub going: Option<bool>,
}

impl Panel {
    pub fn new(side: &'static str, src: Arc<dyn Source>, dir: VPath) -> Panel {
        Panel {
            side,
            src,
            dir,
            listed: Vec::new(),
            items: Vec::new(),
            cursor: 0,
            sort: Sort { key: SortKey::Name, desc: false },
            show_hidden: true,
            seq: 0,
            loading: None,
            error: None,
            failed: None,
            sizes: HashMap::new(),
            space: None,
            sizing: Vec::new(),
            quick: None,
            back: Vec::new(),
            forward: Vec::new(),
            going: None,
        }
    }

    /// The listing of another folder arrived: the one left goes into the history (unless this was a step
    /// through it).
    pub fn remember(&mut self, left: (Arc<dyn Source>, VPath)) {
        const KEEP: usize = 64;
        match self.going.take() {
            Some(true) => {
                self.back.pop();
                self.forward.push(left);
            }
            Some(false) => {
                self.forward.pop();
                self.back.push(left);
            }
            None => {
                self.back.push(left);
                self.forward.clear();
            }
        }
        if self.back.len() > KEEP {
            self.back.remove(0);
        }
    }

    pub fn up_row(&self) -> usize {
        usize::from(!self.dir.is_root())
    }

    pub fn rows(&self) -> usize {
        self.items.len() + self.up_row()
    }

    /// The entry under the cursor; `None` on `..`.
    pub fn current(&self) -> Option<&Entry> {
        self.cursor.checked_sub(self.up_row()).and_then(|i| self.items.get(i)).map(|it| &it.entry)
    }

    pub fn on_up(&self) -> bool {
        self.up_row() == 1 && self.cursor == 0
    }

    /// What an operation applies to: the selection, or the entry under the cursor.
    pub fn targets(&self) -> Vec<Entry> {
        let sel: Vec<Entry> = self.items.iter().filter(|i| i.sel).map(|i| i.entry.clone()).collect();
        if !sel.is_empty() {
            return sel;
        }
        self.current().cloned().into_iter().collect()
    }

    pub fn has_selection(&self) -> bool {
        self.items.iter().any(|i| i.sel)
    }

    /// A fresh listing of `dir`. The cursor goes to `focus` (a name) if given, otherwise stays on the same name;
    /// the selection of names that are still there survives a re-read of the same folder.
    pub fn set_listing(&mut self, dir: VPath, entries: Vec<Entry>, focus: Option<&str>) {
        let same = dir == self.dir;
        let keep_name = focus.map(str::to_string).or_else(|| if same { self.current().map(|e| e.name.clone()) } else { None });
        let selected: HashSet<String> =
            if same { self.items.iter().filter(|i| i.sel).map(|i| i.entry.name.clone()).collect() } else { HashSet::new() };
        if !same {
            self.sizes.clear();
        }
        let old_cursor = self.cursor;
        self.dir = dir;
        self.listed = entries;
        self.error = None;
        self.failed = None;
        self.rebuild(keep_name.as_deref());
        for it in &mut self.items {
            it.sel = selected.contains(&it.entry.name);
        }
        if keep_name.is_none() {
            self.cursor = if same { old_cursor.min(self.rows().saturating_sub(1)) } else { 0 };
        }
    }

    /// Sort and filter again; the cursor follows `name` (or stays on the same entry).
    pub fn rebuild(&mut self, name: Option<&str>) {
        let name = name.map(str::to_string).or_else(|| self.current().map(|e| e.name.clone()));
        let sel: HashSet<String> = self.items.iter().filter(|i| i.sel).map(|i| i.entry.name.clone()).collect();
        let mut items: Vec<Item> =
            self.listed.iter().filter(|e| self.show_hidden || !e.hidden).map(|e| Item { entry: e.clone(), sel: sel.contains(&e.name) }).collect();
        let sort = self.sort;
        items.sort_by(|a, b| compare(&a.entry, &b.entry, sort));
        self.items = items;
        self.cursor = match name {
            Some(n) => self.find(&n).unwrap_or(self.cursor.min(self.rows().saturating_sub(1))),
            None => self.cursor.min(self.rows().saturating_sub(1)),
        };
    }

    /// The row of a name.
    pub fn find(&self, name: &str) -> Option<usize> {
        if name == ".." {
            return (self.up_row() == 1).then_some(0);
        }
        self.items.iter().position(|i| i.entry.name == name).map(|i| i + self.up_row())
    }

    pub fn move_cursor(&mut self, row: isize) {
        let last = self.rows().saturating_sub(1) as isize;
        self.cursor = row.clamp(0, last.max(0)) as usize;
    }

    /// Toggle the selection of the cursor row (not `..`); returns the item index.
    pub fn toggle(&mut self) -> Option<usize> {
        let i = self.cursor.checked_sub(self.up_row())?;
        let it = self.items.get_mut(i)?;
        it.sel = !it.sel;
        Some(i)
    }

    /// The first entry whose name starts with `prefix` (any case, and `й` typed finds `й` stored decomposed
    /// as on HFS+ and SMB), from the top.
    pub fn quick_find(&self, prefix: &str) -> Option<usize> {
        let p = composed(prefix).to_lowercase();
        self.items.iter().position(|i| composed(&i.entry.name).to_lowercase().starts_with(&p)).map(|i| i + self.up_row())
    }

    // ---------------------------------------------------------------- screen data

    fn row(&self, it: &Item) -> Row {
        let e = &it.entry;
        let dir = e.is_dir();
        // A long "extension" (a timestamp, a host name) stays in the name: the column is for short ones.
        let (n, ext) = match split_ext(&e.name) {
            _ if dir => (format!("[{}]", e.name), String::new()),
            (b, x) if x.chars().count() <= MAX_EXT => (b.to_string(), x.to_string()),
            _ => (e.name.clone(), String::new()),
        };
        let size = match (dir, e.size, self.sizes.get(&e.name)) {
            (true, _, Some(s)) => format::bytes(*s),
            (true, _, None) => "<DIR>".into(),
            (false, Some(s), _) => format::bytes(s),
            (false, None, _) => String::new(),
        };
        let link = matches!(e.kind, Kind::Symlink { .. });
        let attr = match e.mode {
            Some(m) => format!("{}{}", if link { "l" } else { "" }, format::mode(m)),
            None => [(e.readonly, 'r'), (e.hidden, 'h'), (link, 'l')].iter().filter(|x| x.0).map(|x| x.1).collect(),
        };
        let icon = crate::icons::src(dir, split_ext(&e.name).1);
        let d = e.modified.map(format::date).unwrap_or_default();
        Row { i: icon, n, e: ext, s: size, d, a: attr, dir, sel: it.sel, hid: e.hidden, t: e.name.clone() }
    }

    fn up_row_data(&self) -> Row {
        Row { i: crate::icons::src(true, ""), n: "[..]".into(), s: "<UP>".into(), dir: true, ..Row::default() }
    }

    /// Everything: rows, cursor, header, status.
    pub fn write(&self, d: &mut UiData, active: bool) {
        let mut rows = Vec::with_capacity(self.rows());
        if self.up_row() == 1 {
            rows.push(self.up_row_data());
        }
        rows.extend(self.items.iter().map(|it| self.row(it)));
        d.set(&format!("{}.rows", self.side), rows);
        self.write_cursor(d);
        self.write_status(d, active);
    }

    pub fn write_cursor(&self, d: &mut UiData) {
        d.set(&format!("{}.cursor", self.side), self.cursor as f64);
        let q = self.quick.clone().unwrap_or_default();
        d.set(&format!("{}.quick", self.side), q);
        d.set(&format!("{}.quickOn", self.side), self.quick.is_some());
    }

    /// One row after a selection change or a counted size: no need to rebuild 50 000 rows.
    pub fn write_row(&self, d: &mut UiData, i: usize) {
        if let Some(it) = self.items.get(i) {
            d.set(&format!("{}.rows.{}", self.side, i + self.up_row()), self.row(it));
        }
    }

    pub fn write_status(&self, d: &mut UiData, active: bool) {
        let s = self.side;
        // The source's symbol in the title: white on the accent color, the theme's ink otherwise.
        let sym = source_symbol(&self.src.info().id);
        d.set(&format!("{s}.iconW"), crate::icons::symbol_white(sym));
        d.set(&format!("{s}.iconL"), crate::icons::symbol(sym, false));
        d.set(&format!("{s}.iconD"), crate::icons::symbol(sym, true));
        // A long path loses its start, not its end: where you are is the last part.
        let full = format!("{}: {}", self.src.info().title, self.dir);
        let room = ((d.num("env.width") as f32 / 2.0 - 24.0) / 7.2).max(16.0) as usize;
        let n = full.chars().count();
        let title = if n > room { format!("…{}", full.chars().skip(n - room + 1).collect::<String>()) } else { full };
        d.set(&format!("{s}.title"), title);
        // The same as pieces: the source, then `/` `docs` `/` `archive` — names that open their folder when
        // clicked. A long path loses its first names to "…".
        let label = format!("{}: ", self.src.info().title);
        let names: Vec<&str> = self.dir.components().collect();
        let mut used = label.chars().count() + 1;
        let mut first = names.len();
        while first > 0 && used + names[first - 1].chars().count() < room {
            used += names[first - 1].chars().count() + 1;
            first -= 1;
        }
        let crumb = |t: &str, path: Option<String>| {
            uison::ui::obj([
                ("t", uison::ui::Json::from(t.to_string())),
                ("go", uison::ui::Json::from(path.is_some())),
                ("p", uison::ui::Json::from(path.unwrap_or_default())),
            ])
        };
        let mut crumbs = vec![if first > 0 { crumb("…/", None) } else { crumb("/", Some("/".into())) }];
        let mut path = String::new();
        for (i, name) in names.iter().enumerate() {
            path.push('/');
            path.push_str(name);
            if i < first {
                continue;
            }
            if i > first {
                crumbs.push(crumb("/", None));
            }
            crumbs.push(crumb(name, Some(path.clone())));
        }
        d.set(&format!("{s}.srcLabel"), label);
        d.set(&format!("{s}.crumbs"), uison::ui::Json::Array(crumbs));
        d.set(&format!("{s}.empty"), self.items.is_empty() && self.loading.is_none() && self.error.is_none());
        d.set(&format!("{s}.active"), active);
        if active {
            // The key bar dims what the active panel's source can't do.
            d.set("keys.ro", !self.src.info().caps.write);
        }
        d.set(&format!("{s}.loading"), self.loading.is_some());
        d.set(&format!("{s}.error"), self.error.clone().unwrap_or_default());
        d.set(&format!("{s}.sort"), self.sort.key.name());
        // Column titles, the sorted one with its direction.
        for (k, title) in [(SortKey::Name, t!("Name")), (SortKey::Ext, t!("Ext")), (SortKey::Size, t!("Size")), (SortKey::Time, t!("Date"))] {
            let mark = match (self.sort.key == k, self.sort.desc) {
                (false, _) => "",
                (true, false) => " ▲",
                (true, true) => " ▼",
            };
            d.set(&format!("{s}.h.{}", k.name()), format!("{title}{mark}"));
        }
        d.set(&format!("{s}.desc"), self.sort.desc);
        let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
        let (mut sfiles, mut sdirs, mut sbytes) = (0u64, 0u64, 0u64);
        for it in &self.items {
            let e = &it.entry;
            let size = if e.is_dir() { self.sizes.get(&e.name).copied().unwrap_or(0) } else { e.size.unwrap_or(0) };
            if e.is_dir() {
                dirs += 1;
            } else {
                files += 1;
            }
            bytes += size;
            if it.sel {
                if e.is_dir() {
                    sdirs += 1;
                } else {
                    sfiles += 1;
                }
                sbytes += size;
            }
        }
        // Plain words: what is here, and what of it is selected.
        let what = |f: u64, d: u64| match (f, d) {
            (_, 0) => format::count(f, "file"),
            (0, _) => format::count(d, "folder"),
            _ => format!("{}, {}", format::count(f, "file"), format::count(d, "folder")),
        };
        let mut status = if sfiles + sdirs == 0 {
            if files + dirs == 0 { t!("Empty").to_string() } else { format!("{} · {}", what(files, dirs), format::short(bytes)) }
        } else {
            t!("Selected {} ({}) of {} ({})", what(sfiles, sdirs), format::short(sbytes), what(files, dirs), format::short(bytes))
        };
        // The volume's free space, as the drive bar of TC shows it.
        if let Some((free, total)) = self.space {
            status.push_str(&format!("  ·  {}", t!("{} free of {}", format::short(free), format::short(total))));
        }
        d.set(&format!("{s}.status"), status);
    }
}

/// The SF Symbol of a kind of source, by its id.
fn source_symbol(id: &str) -> &'static str {
    match id.split(':').next().unwrap_or("") {
        "local" => "internaldrive",
        "s3" => "cloud",
        "sftp" => "server.rack",
        "webdav" => "globe",
        "ftp" | "ftps" => "network",
        "mem" => "memorychip",
        _ => "folder",
    }
}

/// A name in composed form where it has combining marks (decomposed, as HFS+ and SMB keep names); others as
/// they are — they are almost all, and cheap to pass.
fn composed(s: &str) -> std::borrow::Cow<'_, str> {
    #[cfg(target_os = "macos")]
    if s.chars().any(|c| ('\u{300}'..='\u{36f}').contains(&c)) {
        return std::borrow::Cow::Owned(crate::macos::nfc(s));
    }
    std::borrow::Cow::Borrowed(s)
}

/// Folders first (in any direction), then by the key; folders by name unless sorted by time (as in TC).
fn compare(a: &Entry, b: &Entry, sort: Sort) -> Ordering {
    let (da, db) = (a.is_dir(), b.is_dir());
    if da != db {
        return db.cmp(&da);
    }
    let by_name = || name_cmp(&a.name, &b.name);
    let ord = match sort.key {
        SortKey::Name => by_name(),
        SortKey::Ext if !da => split_ext(&a.name).1.to_lowercase().cmp(&split_ext(&b.name).1.to_lowercase()).then_with(by_name),
        SortKey::Size if !da => b.size.cmp(&a.size).then_with(by_name),
        SortKey::Time => b.modified.cmp(&a.modified).then_with(by_name),
        _ => by_name(),
    };
    if sort.desc { ord.reverse() } else { ord }
}

/// Case-insensitive, numbers by value: `file2` before `file10`.
fn name_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(&c) = it.peek().filter(|c| c.is_ascii_digit()) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (n, m) = (take(&mut x), take(&mut y));
                let (nt, mt) = (n.trim_start_matches('0'), m.trim_start_matches('0'));
                let o = nt.len().cmp(&mt.len()).then_with(|| nt.cmp(mt));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs::MemFs;

    fn panel() -> Panel {
        let fs = MemFs::new("t");
        fs.add_file("/d/b.txt", "22")
            .add_file("/d/A.md", "1")
            .add_file("/d/file10", "")
            .add_file("/d/file2", "")
            .add_dir("/d/zdir")
            .add_dir("/d/Adir");
        fs.add_file("/d/.hidden", "");
        let mut p = Panel::new("left", Arc::new(fs), VPath::root());
        let list = p.src.list(&VPath::new("/d"), &Cancel::new()).unwrap();
        p.set_listing(VPath::new("/d"), list, None);
        p
    }

    fn names(p: &Panel) -> Vec<&str> {
        p.items.iter().map(|i| i.entry.name.as_str()).collect()
    }

    #[test]
    fn sorting_as_in_tc() {
        let mut p = panel();
        assert_eq!(names(&p), ["Adir", "zdir", ".hidden", "A.md", "b.txt", "file2", "file10"]);
        p.sort = Sort { key: SortKey::Ext, desc: false };
        p.rebuild(None);
        assert_eq!(names(&p), ["Adir", "zdir", ".hidden", "file2", "file10", "A.md", "b.txt"]);
        p.sort = Sort { key: SortKey::Size, desc: false };
        p.rebuild(None);
        assert_eq!(&names(&p)[..4], ["Adir", "zdir", "b.txt", "A.md"]);
        p.sort = Sort { key: SortKey::Name, desc: true };
        p.rebuild(None);
        assert_eq!(names(&p), ["zdir", "Adir", "file10", "file2", "b.txt", "A.md", ".hidden"], "folders stay on top");
        p.show_hidden = false;
        p.sort = Sort { key: SortKey::Name, desc: false };
        p.rebuild(None);
        assert!(!names(&p).contains(&".hidden"));
    }

    #[test]
    fn cursor_and_selection_survive_a_reread() {
        let mut p = panel();
        p.cursor = p.find("b.txt").unwrap();
        p.toggle();
        assert_eq!(p.targets().len(), 1);
        let list = p.src.list(&VPath::new("/d"), &Cancel::new()).unwrap();
        p.set_listing(VPath::new("/d"), list, None);
        assert_eq!(p.current().unwrap().name, "b.txt");
        assert!(p.has_selection());
        // Entering another folder starts at the top with nothing selected.
        p.set_listing(VPath::new("/d/zdir"), Vec::new(), None);
        assert_eq!((p.cursor, p.has_selection(), p.on_up()), (0, false, true));
        assert_eq!(p.quick_find("FI"), None);
    }

    #[test]
    fn quick_search_finds_decomposed_names() {
        let entries = vec![Entry::new("\u{438}\u{306}ога.txt", vfs::Kind::File), Entry::new("other", vfs::Kind::File)];
        let mut p = Panel::new("left", Arc::new(MemFs::new("t")), VPath::root());
        p.set_listing(VPath::root(), entries, None);
        let found = p.quick_find("Йо");
        if cfg!(target_os = "macos") {
            assert!(found.is_some_and(|r| p.items[r].entry.name.starts_with('\u{438}')), "{found:?}");
        }
    }

    #[test]
    fn quick_search() {
        let p = panel();
        assert_eq!(p.quick_find("FI").map(|r| p.items[r - 1].entry.name.as_str()), Some("file2"));
    }

    /// Everything selected in a big folder, then read again and sorted again: the selection is carried by
    /// name, which once compared every name with every other (2·10⁹ compares for 50 000).
    #[test]
    fn a_big_selection_survives_quickly() {
        let entries: Vec<Entry> = (0..20_000).map(|i| Entry::new(format!("file_{i:05}"), vfs::Kind::File)).collect();
        let mut p = Panel::new("left", Arc::new(MemFs::new("t")), VPath::new("/d"));
        p.set_listing(VPath::new("/d"), entries.clone(), None);
        for it in &mut p.items {
            it.sel = true;
        }
        let t = std::time::Instant::now();
        p.set_listing(VPath::new("/d"), entries, None);
        p.sort = Sort { key: SortKey::Name, desc: true };
        p.rebuild(None);
        let took = t.elapsed();
        assert!(p.items.iter().all(|i| i.sel));
        assert!(took.as_millis() < 500, "{took:?}");
    }
}
