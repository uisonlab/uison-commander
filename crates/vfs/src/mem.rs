//! A source in memory: tests of operations and UI scenarios. It can be slow and fail on purpose,
//! to check that the UI stays responsive, that cancellation works and errors in the middle are handled.

use std::collections::{BTreeMap, HashSet};
use std::io::{self, Cursor, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::*;

#[derive(Clone)]
enum Node {
    Dir,
    File(Arc<Vec<u8>>),
    /// A symbolic link to this absolute path (in the same MemFs).
    Link(VPath),
}

#[derive(Clone)]
struct Item {
    /// The path as it was written (the key may be lower-cased).
    path: VPath,
    node: Node,
    modified: SystemTime,
    mode: u32,
}

impl Item {
    fn new(path: VPath, node: Node) -> Item {
        let mode = if matches!(node, Node::Dir) { 0o755 } else { 0o644 };
        Item { path, node, modified: SystemTime::now(), mode }
    }
}

struct State {
    /// Every object by its key — the full path, lower-cased when names ignore case; the root is implied.
    items: BTreeMap<VPath, Item>,
    fail: HashSet<VPath>,
    ignore_case: bool,
    /// Writers created and not yet finished or dropped.
    writers: usize,
}

impl State {
    fn key(&self, p: &VPath) -> VPath {
        if self.ignore_case { VPath::new(&p.as_str().to_lowercase()) } else { p.clone() }
    }

    fn get(&self, p: &VPath) -> Option<&Item> {
        self.items.get(&self.key(p))
    }

    fn has(&self, p: &VPath) -> bool {
        self.items.contains_key(&self.key(p))
    }

    /// `p` with the links on its way followed (the last one too if `last`), as the system resolves a path.
    fn resolve(&self, p: &VPath, last: bool) -> VPath {
        let mut cur = VPath::root();
        let parts: Vec<&str> = p.components().collect();
        let mut hops = 0;
        for (i, c) in parts.iter().enumerate() {
            cur = cur.child(c);
            if i + 1 == parts.len() && !last {
                break;
            }
            while let Some(Item { node: Node::Link(t), .. }) = self.get(&cur) {
                hops += 1;
                if hops > 32 {
                    return cur;
                }
                cur = t.clone();
            }
        }
        cur
    }

    fn put(&mut self, it: Item) {
        let k = self.key(&it.path);
        self.items.insert(k, it);
    }
}

pub struct MemFs {
    id: String,
    caps: Capabilities,
    delay: Duration,
    /// `rename` is declared but says `Unsupported` (like the local disk between two volumes).
    rename_unsupported: bool,
    st: Arc<Mutex<State>>,
}

impl MemFs {
    pub fn new(id: &str) -> MemFs {
        let caps = Capabilities {
            write: true,
            rename: true,
            server_copy: false,
            real_dirs: true,
            seek: true,
            permissions: true,
            set_mtime: true,
            case_sensitive: true,
        };
        let st = State { items: BTreeMap::new(), fail: HashSet::new(), ignore_case: false, writers: 0 };
        MemFs { id: id.into(), caps, delay: Duration::ZERO, rename_unsupported: false, st: Arc::new(Mutex::new(st)) }
    }

    /// `A.txt` and `a.txt` are one file, as on macOS and Windows volumes. Call before adding anything.
    pub fn ignore_case(mut self) -> MemFs {
        self.caps.case_sensitive = false;
        self.st.lock().unwrap().ignore_case = true;
        self
    }

    /// `rename` is declared, but every call says `Unsupported`: `ops` has to take its fallback.
    pub fn rename_unsupported(mut self) -> MemFs {
        self.rename_unsupported = true;
        self
    }

    /// Writers created and neither finished nor dropped — for tests of cancellation.
    pub fn open_writers(&self) -> usize {
        self.st.lock().unwrap().writers
    }

    /// Every listing, stat, open, read and the roots wait this long (a slow network).
    pub fn slow(mut self, ms: u64) -> MemFs {
        self.delay = Duration::from_millis(ms);
        self
    }

    /// Without `rename`: moves have to take the copy-and-delete path (like S3).
    pub fn without_rename(mut self) -> MemFs {
        self.caps.rename = false;
        self
    }

    pub fn read_only(mut self) -> MemFs {
        self.caps.write = false;
        self
    }

    /// Listing, opening, creating, removing or renaming `path` fails with an I/O error (until `heal`).
    pub fn fail_on(&self, path: &str) {
        let mut st = self.st.lock().unwrap();
        let k = st.key(&VPath::new(path));
        st.fail.insert(k);
    }

    pub fn heal(&self) {
        self.st.lock().unwrap().fail.clear();
    }

    /// Put a file, creating its folders.
    pub fn add_file(&self, path: &str, data: impl Into<Vec<u8>>) -> &MemFs {
        let p = VPath::new(path);
        self.add_dirs(p.parent().as_ref());
        self.st.lock().unwrap().put(Item::new(p, Node::File(Arc::new(data.into()))));
        self
    }

    /// A symbolic link at `path` to the absolute `target`, making the folders of `path`.
    pub fn add_link(&self, path: &str, target: &str) -> &MemFs {
        let p = VPath::new(path);
        self.add_dirs(p.parent().as_ref());
        self.st.lock().unwrap().put(Item::new(p, Node::Link(VPath::new(target))));
        self
    }

    pub fn add_dir(&self, path: &str) -> &MemFs {
        self.add_dirs(Some(&VPath::new(path)));
        self
    }

    pub fn set_modified(&self, path: &str, t: SystemTime) {
        let mut st = self.st.lock().unwrap();
        let k = st.key(&VPath::new(path));
        if let Some(it) = st.items.get_mut(&k) {
            it.modified = t;
        }
    }

    /// File contents — for tests.
    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        match &self.st.lock().unwrap().get(&VPath::new(path))?.node {
            Node::File(d) => Some(d.to_vec()),
            Node::Dir | Node::Link(_) => None,
        }
    }

    pub fn exists(&self, path: &str) -> bool {
        let p = VPath::new(path);
        p.is_root() || self.st.lock().unwrap().has(&p)
    }

    /// All paths as written, sorted by key — for tests.
    pub fn paths(&self) -> Vec<String> {
        self.st.lock().unwrap().items.values().map(|it| it.path.to_string()).collect()
    }

    fn add_dirs(&self, dir: Option<&VPath>) {
        let Some(dir) = dir else { return };
        let mut st = self.st.lock().unwrap();
        let mut p = VPath::root();
        for c in dir.components() {
            p = p.child(c);
            if !st.has(&p) {
                st.put(Item::new(p.clone(), Node::Dir));
            }
        }
    }

    fn wait(&self) {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
    }

    fn check_fail(st: &State, p: &VPath) -> Result<()> {
        if st.fail.contains(&st.key(p)) { Err(Error::msg(p, "simulated failure")) } else { Ok(()) }
    }

    fn entry(st: &State, it: &Item) -> Entry {
        let (kind, size) = match &it.node {
            Node::Dir => (Kind::Dir, None),
            Node::File(d) => (Kind::File, Some(d.len() as u64)),
            // As on a disk: the link's own kind, with the size of what it leads to.
            Node::Link(t) => {
                let real = st.resolve(t, true);
                let size = match st.get(&real).map(|i| &i.node) {
                    Some(Node::File(d)) => Some(d.len() as u64),
                    _ => None,
                };
                (Kind::Symlink { target: Some(t.to_string()), to_dir: Self::is_dir(st, &real) }, size)
            }
        };
        let name = it.path.name().to_string();
        Entry {
            hidden: name.starts_with('.'),
            name,
            kind,
            size,
            modified: Some(it.modified),
            mode: Some(it.mode),
            readonly: it.mode & 0o200 == 0,
            extra: Vec::new(),
        }
    }

    fn is_dir(st: &State, p: &VPath) -> bool {
        p.is_root() || matches!(st.get(p), Some(Item { node: Node::Dir, .. }))
    }

    /// The parent must be an existing folder.
    fn check_parent(st: &State, p: &VPath) -> Result<()> {
        let parent = p.parent().ok_or_else(|| Error::AlreadyExists(p.clone()))?;
        if Self::is_dir(st, &parent) {
            Ok(())
        } else if st.has(&parent) {
            Err(Error::NotADirectory(parent))
        } else {
            Err(Error::NotFound(parent))
        }
    }
}

struct MemWriter {
    st: Arc<Mutex<State>>,
    path: VPath,
    buf: Vec<u8>,
    opts: CreateOpts,
}

impl Write for MemWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Writer for MemWriter {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        MemFs::check_fail(&st, &self.path)?;
        MemFs::check_parent(&st, &self.path)?;
        let path = st.get(&self.path).map_or_else(|| self.path.clone(), |it| it.path.clone());
        match st.get(&self.path) {
            Some(Item { node: Node::Dir, .. }) => return Err(Error::IsADirectory(self.path.clone())),
            Some(_) if !self.opts.overwrite => return Err(Error::AlreadyExists(self.path.clone())),
            _ => {}
        }
        // Overwriting keeps the name as it is (another case of it is the same file).
        let mut it = Item::new(path, Node::File(Arc::new(std::mem::take(&mut self.buf))));
        if let Some(t) = self.opts.modified {
            it.modified = t;
        }
        if let Some(m) = self.opts.mode {
            it.mode = m;
        }
        st.put(it);
        Ok(())
    }
}

impl Drop for MemWriter {
    fn drop(&mut self) {
        self.st.lock().unwrap().writers -= 1;
    }
}

impl Source for MemFs {
    fn info(&self) -> SourceInfo {
        SourceInfo { id: format!("mem:{}", self.id), title: format!("mem:{}", self.id), caps: self.caps }
    }

    fn roots(&self, cancel: &Cancel) -> Result<Vec<Root>> {
        self.wait();
        cancel.check()?;
        Ok(vec![Root { path: VPath::root(), title: "/".into() }])
    }

    fn home(&self) -> VPath {
        VPath::root()
    }

    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>> {
        self.wait();
        cancel.check()?;
        let st = self.st.lock().unwrap();
        Self::check_fail(&st, dir)?;
        let real = st.resolve(dir, true);
        if !Self::is_dir(&st, &real) {
            return Err(if st.has(&real) { Error::NotADirectory(dir.clone()) } else { Error::NotFound(dir.clone()) });
        }
        let k = st.key(&real);
        Ok(st.items.iter().filter(|(p, _)| p.parent().as_ref() == Some(&k)).map(|(_, it)| Self::entry(&st, it)).collect())
    }

    fn stat(&self, path: &VPath) -> Result<Entry> {
        self.wait();
        if path.is_root() {
            return Ok(Entry::new("", Kind::Dir));
        }
        let st = self.st.lock().unwrap();
        // As lstat: a link itself, but the links on the way followed.
        let real = st.resolve(path, false);
        st.get(&real).map(|it| Self::entry(&st, it)).ok_or_else(|| Error::NotFound(path.clone()))
    }

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        self.wait();
        let st = self.st.lock().unwrap();
        Self::check_fail(&st, path)?;
        let real = st.resolve(path, true);
        match st.get(&real).map(|it| &it.node) {
            Some(Node::File(d)) => {
                let mut c = Cursor::new(d.clone());
                c.set_position(offset);
                Ok(Box::new(ArcReader(c, self.delay)))
            }
            Some(Node::Dir) => Err(Error::IsADirectory(path.clone())),
            Some(Node::Link(_)) | None => Err(Error::NotFound(path.clone())),
        }
    }

    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn Writer>> {
        self.wait();
        if !self.caps.write {
            return Err(Error::PermissionDenied(path.clone()));
        }
        let mut st = self.st.lock().unwrap();
        // The links on the way followed; the last one is the object itself (removed, replaced as a link).
        let path = &st.resolve(path, false);
        Self::check_fail(&st, path)?;
        Self::check_parent(&st, path)?;
        match st.get(path) {
            Some(Item { node: Node::Dir, .. }) => return Err(Error::IsADirectory(path.clone())),
            Some(_) if !opts.overwrite => return Err(Error::AlreadyExists(path.clone())),
            _ => {}
        }
        st.writers += 1;
        Ok(Box::new(MemWriter { st: self.st.clone(), path: path.clone(), buf: Vec::new(), opts: opts.clone() }))
    }

    fn mkdir(&self, path: &VPath) -> Result<()> {
        if !self.caps.write {
            return Err(Error::PermissionDenied(path.clone()));
        }
        let mut st = self.st.lock().unwrap();
        // The links on the way followed; the last one is the object itself (removed, replaced as a link).
        let path = &st.resolve(path, false);
        Self::check_fail(&st, path)?;
        Self::check_parent(&st, path)?;
        if st.has(path) {
            return Err(Error::AlreadyExists(path.clone()));
        }
        st.put(Item::new(path.clone(), Node::Dir));
        Ok(())
    }

    fn remove(&self, path: &VPath) -> Result<()> {
        if !self.caps.write {
            return Err(Error::PermissionDenied(path.clone()));
        }
        let mut st = self.st.lock().unwrap();
        // The links on the way followed; the last one is the object itself (removed, replaced as a link).
        let path = &st.resolve(path, false);
        Self::check_fail(&st, path)?;
        let k = st.key(path);
        if !st.items.contains_key(&k) {
            return Err(Error::NotFound(path.clone()));
        }
        if st.items.keys().any(|p| p.parent().as_ref() == Some(&k)) {
            return Err(Error::DirNotEmpty(path.clone()));
        }
        st.items.remove(&k);
        Ok(())
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        if !self.caps.rename || self.rename_unsupported {
            return Err(Error::Unsupported);
        }
        let mut st = self.st.lock().unwrap();
        let (from, to) = (&st.resolve(from, false), &st.resolve(to, false));
        Self::check_fail(&st, from)?;
        let (kf, kt) = (st.key(from), st.key(to));
        if !st.items.contains_key(&kf) {
            return Err(Error::NotFound(from.clone()));
        }
        // Another case of the same name is the object itself: a case-only rename.
        if st.items.contains_key(&kt) && kt != kf {
            return Err(Error::AlreadyExists(to.clone()));
        }
        if kt != kf && kt.starts_with(&kf) {
            return Err(Error::msg(to, "cannot move a folder into itself"));
        }
        Self::check_parent(&st, to)?;
        let moved: Vec<VPath> = st.items.keys().filter(|p| p.starts_with(&kf)).cloned().collect();
        let from_shown = st.items[&kf].path.clone();
        for k in moved {
            let mut it = st.items.remove(&k).unwrap();
            let rest = it.path.strip_prefix(&from_shown).unwrap_or("").to_string();
            it.path = if rest.is_empty() { to.clone() } else { to.join(&rest) };
            st.put(it);
        }
        Ok(())
    }

    fn set_mtime(&self, path: &VPath, t: SystemTime) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        // The links on the way followed; the last one is the object itself (removed, replaced as a link).
        let path = &st.resolve(path, false);
        Self::check_fail(&st, path)?;
        let k = st.key(path);
        let it = st.items.get_mut(&k).ok_or_else(|| Error::NotFound(path.clone()))?;
        it.modified = t;
        Ok(())
    }

    fn set_mode(&self, path: &VPath, mode: u32) -> Result<()> {
        let mut st = self.st.lock().unwrap();
        // The links on the way followed; the last one is the object itself (removed, replaced as a link).
        let path = &st.resolve(path, false);
        Self::check_fail(&st, path)?;
        let k = st.key(path);
        let it = st.items.get_mut(&k).ok_or_else(|| Error::NotFound(path.clone()))?;
        it.mode = mode;
        Ok(())
    }

    fn same_file(&self, a: &VPath, b: &VPath) -> bool {
        let st = self.st.lock().unwrap();
        st.key(&st.resolve(a, false)) == st.key(&st.resolve(b, false))
    }

    /// A stand-in for S3's "Copy web link": proves the path action → parameters dialog → clipboard.
    fn actions(&self, _dir: &VPath, sel: &[Entry]) -> Vec<ActionInfo> {
        if sel.is_empty() {
            return Vec::new();
        }
        let scheme = Param::Choice {
            id: "scheme".into(),
            title: "Link type".into(),
            options: vec![("mem".into(), "mem://".into()), ("http".into(), "http://localhost".into())],
            default: "mem".into(),
        };
        vec![ActionInfo { id: "mem.copy-link".into(), title: "Copy link".into(), multi: true, params: vec![scheme] }]
    }

    fn run_action(&self, id: &str, sel: &[VPath], args: &ActionArgs, cancel: &Cancel) -> Result<ActionResult> {
        cancel.check()?;
        if id != "mem.copy-link" {
            return Err(Error::Unsupported);
        }
        let base = if args.get("scheme") == Some("http") { "http://localhost".to_string() } else { format!("mem://{}", self.id) };
        let links: Vec<String> = sel.iter().map(|p| format!("{base}{p}")).collect();
        Ok(ActionResult::Clipboard(links.join("\n")))
    }
}

/// Shared bytes and the delay of every read (a slow source is slow to read too).
struct ArcReader(Cursor<Arc<Vec<u8>>>, Duration);

impl Read for ArcReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.1.is_zero() {
            std::thread::sleep(self.1);
        }
        let pos = self.0.position() as usize;
        let data = self.0.get_ref();
        let n = buf.len().min(data.len().saturating_sub(pos));
        buf[..n].copy_from_slice(&data[pos..pos + n]);
        self.0.set_position((pos + n) as u64);
        Ok(n)
    }
}
