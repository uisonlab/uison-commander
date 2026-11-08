//! Operations over sources, without UI: copy, move, delete, make a folder, count a folder's size.
//!
//! An operation runs in a worker thread. It reports progress and asks questions (a file exists, a file
//! failed) through [`Feedback`]; `ask` blocks the worker until the user answers. Everything goes through
//! `vfs::Source`, so local → local and local → S3 are the same code.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use vfs::{Cancel, CreateOpts, Entry, Error, Kind, Result, Source, VPath, same_storage};

mod mask;

pub use mask::{apply_mask, is_mask, matches_mask};

/// One block of a streamed copy; cancellation is checked between blocks.
pub const BLOCK: usize = 1 << 20;
/// Progress is reported no more often than this.
const PROGRESS_EVERY: Duration = Duration::from_millis(50);
/// Folders nested deeper than this are not walked (a loop of links, a broken source).
const MAX_DEPTH: usize = 256;
/// FAT keeps times to 2 s, SMB and S3 to 1 s: closer than this is "the same time" for "overwrite older".
const TIME_SLACK: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    /// Still walking the folders to count what is to be done.
    pub planning: bool,
    /// The object being processed now.
    pub current: String,
    pub file_done: u64,
    pub file_size: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_done: u64,
    pub files_total: u64,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)] // one question per conflict: size doesn't matter
pub enum Question {
    /// The target exists: `src` is what comes, `dst` is what is there.
    Exists { path: VPath, src: Entry, dst: Entry },
    /// Something failed; `Retry` makes sense.
    Failed { path: VPath, error: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Overwrite,
    OverwriteAll,
    /// Overwrite this and all further targets that are older than what comes; skip the rest.
    OverwriteOlder,
    Skip,
    SkipAll,
    /// Copy under another name (in the same folder).
    Rename(String),
    Retry,
    Cancel,
}

/// The UI side of an operation. Called from the worker thread.
pub trait Feedback: Sync {
    fn progress(&self, p: &Progress);
    fn ask(&self, q: Question) -> Answer;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Files (and folders) done.
    pub done: u64,
    pub skipped: u64,
    pub failed: u64,
    pub cancelled: bool,
    /// What went wrong, for a person; also what was skipped without asking (links to folders inside a copied tree).
    pub errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Copy,
    Move,
}

/// Copy or move `names` from `src_dir` to `target`, a path in `dst` as the user typed it in the dialog:
/// - an existing folder, or ending with '/': into it;
/// - a mask (`*.bak`): into its folder, names through the mask;
/// - one object and a path that is not a folder: under that name (rename);
/// - several objects and a path that doesn't exist: into a new folder of that name.
pub struct Transfer {
    pub mode: Mode,
    pub src: Arc<dyn Source>,
    pub src_dir: VPath,
    pub names: Vec<String>,
    pub dst: Arc<dyn Source>,
    pub target: String,
}

enum Dest {
    Into(VPath),
    Rename(VPath, String),
}

impl Dest {
    fn of(&self, name: &str) -> VPath {
        match self {
            Dest::Into(d) => d.child(name),
            Dest::Rename(d, m) if is_mask(m) => d.child(&apply_mask(name, m)),
            Dest::Rename(d, m) => d.child(m),
        }
    }

    fn dir(&self) -> &VPath {
        match self {
            Dest::Into(d) | Dest::Rename(d, _) => d,
        }
    }
}

enum Step {
    /// Make the folder (an existing one is fine: the trees merge).
    Dir { dst: VPath },
    File {
        src: VPath,
        dst: VPath,
        entry: Entry,
        /// Copied, not moved: it lies behind a moved link to a folder, and the folder is not ours to empty.
        keep: bool,
    },
    /// After its contents (move): remove the source folder if it is empty now.
    DirDone { src: VPath },
    /// A moved link to a folder, after the folder's contents were copied: remove the link itself, if nothing
    /// in this item failed or was skipped.
    Unlink { src: VPath },
}

/// A selected object and what doing it takes.
struct Item {
    src: VPath,
    dst: VPath,
    entry: Entry,
    /// Moved whole by one rename: not walked.
    whole: bool,
    steps: Vec<Step>,
}

#[derive(Default)]
struct Policy {
    /// Names at the target differ by case (asked of the target folder once).
    cs: bool,
    overwrite_all: bool,
    skip_all: bool,
    older: bool,
    skip_errors: bool,
}

/// Progress with a rate limit.
struct Meter<'a> {
    fb: &'a dyn Feedback,
    p: Progress,
    sent: Option<Instant>,
}

impl Meter<'_> {
    fn send(&mut self, force: bool) {
        if force || self.sent.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
            self.fb.progress(&self.p);
            self.sent = Some(Instant::now());
        }
    }
}

/// Compare paths the way the storage does.
fn same_path(a: &VPath, b: &VPath, case_sensitive: bool) -> bool {
    if case_sensitive { a == b } else { a.as_str().to_lowercase() == b.as_str().to_lowercase() }
}

fn inside(p: &VPath, base: &VPath, case_sensitive: bool) -> bool {
    if case_sensitive { p.starts_with(base) } else { VPath::new(&p.as_str().to_lowercase()).starts_with(&VPath::new(&base.as_str().to_lowercase())) }
}

impl Transfer {
    pub fn run(&self, fb: &dyn Feedback, cancel: &Cancel) -> Report {
        // The sources see the cancel in every call, not only those that take it.
        cancel.scoped(|| self.run_scoped(fb, cancel))
    }

    fn run_scoped(&self, fb: &dyn Feedback, cancel: &Cancel) -> Report {
        let mut r = Report::default();
        let mut meter = Meter { fb, p: Progress { planning: true, ..Progress::default() }, sent: None };
        meter.send(true);
        match self.run_inner(&mut meter, &mut r, cancel) {
            Ok(()) => {}
            Err(Error::Cancelled) => r.cancelled = true,
            Err(e) => {
                r.failed += 1;
                r.errors.push(e.to_string());
            }
        }
        r
    }

    fn same(&self) -> bool {
        same_storage(&*self.src, &*self.dst)
    }

    fn resolve(&self) -> Result<Dest> {
        let t = self.target.trim();
        if t.is_empty() {
            return Err(Error::Io { path: None, msg: "no target".into() });
        }
        let p = VPath::new(t);
        if t.ends_with('/') || p.is_root() {
            return Ok(Dest::Into(p));
        }
        if self.dst.stat(&p).is_ok_and(|e| e.is_dir()) {
            return Ok(Dest::Into(p));
        }
        let parent = p.parent().unwrap_or_else(VPath::root);
        let name = p.name().to_string();
        if is_mask(&name) || self.names.len() == 1 { Ok(Dest::Rename(parent, name)) } else { Ok(Dest::Into(p)) }
    }

    fn run_inner(&self, m: &mut Meter, r: &mut Report, cancel: &Cancel) -> Result<()> {
        let dest = self.resolve()?;
        let caps_src = self.src.info().caps;
        let caps_dst = self.dst.info().caps;
        if !caps_dst.write || (self.mode == Mode::Move && !caps_src.write) {
            return Err(Error::Io {
                path: None,
                msg: format!("{} is read-only", if caps_dst.write { self.src.info().title } else { self.dst.info().title }),
            });
        }
        let same = self.same();
        let cs = self.dst.case_sensitive(dest.dir());
        // Checks before anything is touched.
        for n in &self.names {
            let src = self.src_dir.child(n);
            let dst = dest.of(n);
            if same && inside(&dst, &src, cs) && !same_path(&dst, &src, cs) {
                return Err(Error::msg(&src, format!("cannot {} a folder into itself", if self.mode == Mode::Copy { "copy" } else { "move" })));
            }
            if same && self.mode == Mode::Copy && (same_path(&dst, &src, cs) || self.src.same_file(&src, &dst)) {
                return Err(Error::msg(&src, "cannot copy a file onto itself"));
            }
        }
        // The target folder may not exist yet ("several objects into a new folder", "F5 to a typed path").
        self.make_dirs(dest.dir())?;
        // Plan: walk what has to be walked, count files and bytes.
        let rename_ok = self.mode == Mode::Move && same && caps_src.rename;
        let mut policy = Policy { cs, ..Policy::default() };
        let mut items = Vec::new();
        for n in &self.names {
            cancel.check()?;
            let src = self.src_dir.child(n);
            let dst = dest.of(n);
            let entry = match self.src.stat(&src) {
                Ok(e) => e,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    r.failed += 1;
                    r.errors.push(e.to_string());
                    continue;
                }
            };
            m.p.current = src.to_string();
            m.send(false);
            // The target is the object itself under another path (a link in the path, a hard link): nothing to move.
            if same && self.alias(&src, &dst, cs) {
                r.skipped += 1;
                r.errors.push(format!("{src}: {dst} is the same object"));
                continue;
            }
            // A whole move by rename when nothing is in the way (or only a case change).
            let free = self.dst.stat(&dst).is_err() || (!cs && same_path(&dst, &src, cs));
            if rename_ok && free {
                m.p.files_total += 1;
                items.push(Item { src, dst, entry, whole: true, steps: Vec::new() });
                continue;
            }
            let mut steps = Vec::new();
            self.walk(&src, &dst, &entry, Walk { depth: 0, top: true, keep: false }, &mut steps, &mut policy, m, r, cancel)?;
            items.push(Item { src, dst, entry, whole: false, steps });
        }
        m.p.planning = false;
        m.send(true);
        for it in items {
            cancel.check()?;
            if it.whole {
                m.p.current = it.src.to_string();
                m.send(true);
                match retry(m.fb, &mut policy, r, &it.src, true, &mut || self.src.rename(&it.src, &it.dst))? {
                    Outcome::Done => r.done += 1,
                    Outcome::Skipped => {}
                    Outcome::Fallback => {
                        // Not one rename after all (another volume): file by file.
                        let mut steps = Vec::new();
                        if self.src.stat(&it.src).is_ok() {
                            let top = Walk { depth: 0, top: true, keep: false };
                            self.walk(&it.src, &it.dst, &it.entry, top, &mut steps, &mut policy, m, r, cancel)?;
                            self.do_steps(steps, &mut policy, m, r, cancel)?;
                        }
                    }
                }
                m.p.files_done += 1;
                continue;
            }
            self.do_steps(it.steps, &mut policy, m, r, cancel)?;
        }
        m.send(true);
        Ok(())
    }

    fn make_dirs(&self, dir: &VPath) -> Result<()> {
        if self.dst.stat(dir).is_ok() {
            return Ok(());
        }
        let mut p = VPath::root();
        for c in dir.components() {
            p = p.child(c);
            match self.dst.mkdir(&p) {
                Ok(()) | Err(Error::AlreadyExists(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// `src` and `dst` are one object reached by two paths, and not a case-only rename on a volume that
    /// ignores case (that one is a real rename).
    fn alias(&self, src: &VPath, dst: &VPath, cs: bool) -> bool {
        let case_only = !cs && same_path(src, dst, cs) && src != dst;
        src == dst || (!case_only && self.src.same_file(src, dst))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        src: &VPath,
        dst: &VPath,
        e: &Entry,
        w: Walk,
        out: &mut Vec<Step>,
        policy: &mut Policy,
        m: &mut Meter,
        r: &mut Report,
        cancel: &Cancel,
    ) -> Result<()> {
        cancel.check()?;
        let link_dir = matches!(e.kind, Kind::Symlink { to_dir: true, .. });
        let move_same = self.mode == Mode::Move && self.same();
        // A link to a folder inside a copied tree is not followed (it may lead anywhere, even to "/");
        // a selected one is — the user chose it.
        if link_dir && !w.top && !move_same {
            r.skipped += 1;
            r.errors.push(format!("{src}: link to a folder, skipped"));
            return Ok(());
        }
        // A pipe would wait for a writer forever, a device would never end.
        if e.kind == Kind::Other {
            r.skipped += 1;
            r.errors.push(format!("{src}: not a regular file (a pipe, socket or device), skipped"));
            return Ok(());
        }
        if !e.is_dir() || (link_dir && move_same) {
            m.p.files_total += 1;
            m.p.bytes_total += e.size.unwrap_or(0);
            out.push(Step::File { src: src.clone(), dst: dst.clone(), entry: e.clone(), keep: w.keep });
            return Ok(());
        }
        if w.depth > MAX_DEPTH {
            return Err(Error::msg(src, "folders nested too deep"));
        }
        // A moved link to a folder (to another storage): its contents are copied, and only the link goes.
        let keep = w.keep || (link_dir && self.mode == Mode::Move);
        m.p.current = src.to_string();
        m.send(false);
        out.push(Step::Dir { dst: dst.clone() });
        let Some(mut children) = list_or_ask(&*self.src, src, policy, m.fb, r, cancel)? else { return Ok(()) };
        children.sort_by(|a, b| a.name.cmp(&b.name));
        for c in children {
            let inner = Walk { depth: w.depth + 1, top: false, keep };
            self.walk(&src.child(&c.name), &dst.child(&c.name), &c, inner, out, policy, m, r, cancel)?;
        }
        if link_dir && self.mode == Mode::Move {
            out.push(Step::Unlink { src: src.clone() });
        } else if self.mode == Mode::Move && !keep {
            out.push(Step::DirDone { src: src.clone() });
        }
        Ok(())
    }

    fn do_steps(&self, steps: Vec<Step>, policy: &mut Policy, m: &mut Meter, r: &mut Report, cancel: &Cancel) -> Result<()> {
        let trouble = |r: &Report| r.failed + r.skipped;
        let before = trouble(r);
        for s in steps {
            cancel.check()?;
            match s {
                // Skipped, the folder's contents fail one by one: that's what the user chose.
                Step::Dir { dst } => {
                    retry(m.fb, policy, r, &dst, false, &mut || match self.dst.mkdir(&dst) {
                        Err(Error::AlreadyExists(_)) if self.dst.stat(&dst).is_ok_and(|e| e.is_dir()) => Ok(()),
                        x => x,
                    })?;
                }
                Step::File { src, dst, entry, keep } => self.file(&src, &dst, &entry, keep, policy, m, r, cancel)?,
                Step::DirDone { src } => match self.src.remove(&src) {
                    // Something inside was skipped: the folder stays, as it should.
                    Ok(()) | Err(Error::DirNotEmpty(_)) => {}
                    Err(Error::Cancelled) => return Err(Error::Cancelled),
                    Err(e) => {
                        r.failed += 1;
                        r.errors.push(e.to_string());
                    }
                },
                Step::Unlink { src } if trouble(r) == before => {
                    if let Err(e) = self.src.remove(&src) {
                        r.failed += 1;
                        r.errors.push(e.to_string());
                    }
                }
                Step::Unlink { src } => r.errors.push(format!("{src}: the link stays, not everything behind it was copied")),
            }
        }
        Ok(())
    }

    /// One file: conflicts, then rename / clone / stream, then (move) delete the source.
    #[allow(clippy::too_many_arguments)]
    fn file(
        &self,
        src: &VPath,
        dst: &VPath,
        entry: &Entry,
        keep: bool,
        policy: &mut Policy,
        m: &mut Meter,
        r: &mut Report,
        cancel: &Cancel,
    ) -> Result<()> {
        let size = entry.size.unwrap_or(0);
        m.p.current = src.to_string();
        m.p.file_done = 0;
        m.p.file_size = size;
        m.send(true);
        let base = m.p.bytes_done;
        let finish = |m: &mut Meter| {
            m.p.bytes_done = base + size;
            m.p.files_done += 1;
            m.send(false);
        };
        let cs = policy.cs;
        let mut dst = dst.clone();
        // The target is the file itself (the same path, or another path to it): overwriting would destroy it.
        if self.same() && self.alias(src, &dst, cs) {
            if self.mode == Mode::Move {
                r.skipped += 1;
                if *src != dst {
                    r.errors.push(format!("{src}: {dst} is the same file"));
                }
            } else {
                r.failed += 1;
                r.errors.push(format!("{src}: cannot copy a file onto itself ({dst})"));
            }
            finish(m);
            return Ok(());
        }
        loop {
            // The target exists? `agreed` — the user said to replace it.
            let mut agreed = false;
            while let Ok(there) = self.dst.stat(&dst) {
                // A case-only rename on a case-insensitive storage finds the source itself.
                if self.mode == Mode::Move && self.same() && !cs && self.src.same_file(src, &dst) {
                    break;
                }
                if there.is_dir() {
                    r.failed += 1;
                    r.errors.push(format!("{dst}: a folder with this name exists"));
                    finish(m);
                    return Ok(());
                }
                let newer = matches!((entry.modified, there.modified), (Some(a), Some(b)) if a > b + TIME_SLACK);
                let answer = if policy.overwrite_all || (policy.older && newer) {
                    Answer::Overwrite
                } else if policy.skip_all || policy.older {
                    Answer::Skip
                } else {
                    m.fb.ask(Question::Exists { path: dst.clone(), src: entry.clone(), dst: there })
                };
                match answer {
                    Answer::Overwrite | Answer::Retry => {
                        agreed = true;
                        break;
                    }
                    Answer::OverwriteAll => {
                        policy.overwrite_all = true;
                        agreed = true;
                        break;
                    }
                    Answer::OverwriteOlder => {
                        policy.older = true;
                        if newer {
                            agreed = true;
                            break;
                        }
                        r.skipped += 1;
                        finish(m);
                        return Ok(());
                    }
                    Answer::Skip => {
                        r.skipped += 1;
                        finish(m);
                        return Ok(());
                    }
                    Answer::SkipAll => {
                        policy.skip_all = true;
                        r.skipped += 1;
                        finish(m);
                        return Ok(());
                    }
                    // Asked again if the new name is taken too — or is not a name at all ("a/b", "..").
                    Answer::Rename(n) => {
                        if let Some(p) = dst.parent().unwrap_or_else(VPath::root).try_child(n.trim()) {
                            dst = p;
                        }
                    }
                    Answer::Cancel => return Err(Error::Cancelled),
                }
            }
            // Something took the free name meanwhile (another program): back to the question about it.
            let mut taken = false;
            let fb = m.fb;
            let out = retry(fb, policy, r, src, false, &mut || {
                (m.p.bytes_done, m.p.file_done) = (base, 0);
                match self.transfer(src, &dst, entry, agreed, keep, cs, m, cancel) {
                    Err(Error::AlreadyExists(_)) if !agreed => {
                        taken = true;
                        Ok(())
                    }
                    x => x,
                }
            })?;
            if taken {
                continue;
            }
            if out == Outcome::Done {
                r.done += 1;
            }
            finish(m);
            return Ok(());
        }
    }

    /// The data itself. Move inside one storage — rename; otherwise a clone on the server or a stream, and
    /// the source is deleted only after the copy succeeded. `overwrite` — the user agreed to replace `dst`;
    /// without it an existing `dst` is `AlreadyExists`, never replaced.
    #[allow(clippy::too_many_arguments)]
    fn transfer(&self, src: &VPath, dst: &VPath, entry: &Entry, overwrite: bool, keep: bool, cs: bool, m: &mut Meter, cancel: &Cancel) -> Result<()> {
        let same = self.same();
        let caps = self.src.info().caps;
        if self.mode == Mode::Move && same && caps.rename {
            let case_only = !cs && same_path(src, dst, cs);
            match self.rename_over(src, dst, overwrite && !case_only) {
                Err(Error::Unsupported) => {}
                x => return x,
            }
        }
        let mut copied = false;
        if same && caps.server_copy && (overwrite || self.dst.stat(dst).is_err()) {
            match self.src.copy_within(src, dst, cancel) {
                Ok(()) => copied = true,
                Err(Error::Unsupported) => {}
                Err(e) => return Err(e),
            }
        }
        if !copied {
            self.stream(src, dst, entry, overwrite, m, cancel)?;
        }
        if self.mode == Mode::Move && !keep {
            self.src.remove(src)?;
        }
        Ok(())
    }

    /// Rename `src` to `dst`; with `replace`, the existing `dst` first steps aside and comes back if the rename
    /// fails — the old target is never lost before the new one is in place.
    fn rename_over(&self, src: &VPath, dst: &VPath, replace: bool) -> Result<()> {
        if !replace {
            return self.src.rename(src, dst);
        }
        let parent = dst.parent().unwrap_or_else(VPath::root);
        let aside = parent.child(&format!(".uc-aside-{}-{}", std::process::id(), ASIDE.fetch_add(1, Ordering::Relaxed)));
        self.dst.rename(dst, &aside)?;
        match self.src.rename(src, dst) {
            Ok(()) => {
                // The old file is replaced; failing to delete it leaves a hidden leftover, not a lost file.
                let _ = self.dst.remove(&aside);
                Ok(())
            }
            Err(e) => {
                let _ = self.dst.rename(&aside, dst);
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn stream(&self, src: &VPath, dst: &VPath, entry: &Entry, overwrite: bool, m: &mut Meter, cancel: &Cancel) -> Result<()> {
        // The date and permissions are kept where the target can — set by the writer at `finish`.
        let (cs, cd) = (self.src.info().caps, self.dst.info().caps);
        let opts = CreateOpts {
            overwrite,
            size_hint: entry.size,
            modified: entry.modified.filter(|_| cd.set_mtime),
            // As `cp`: setuid and setgid are not carried over.
            mode: entry.mode.filter(|_| cs.permissions && cd.permissions).map(|m| m & 0o1777),
        };
        let mut w = self.dst.create(dst, &opts)?;
        let mut rd = self.src.open_read(src, 0)?;
        let mut buf = vec![0u8; BLOCK];
        loop {
            cancel.check()?;
            let n = match rd.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // A source stopped by the cancel says so as an I/O error: it is the cancel, not a failure to ask about.
                Err(_) if cancel.is_cancelled() => return Err(Error::Cancelled),
                Err(e) => return Err(Error::io(src, &e)),
            };
            w.write_all(&buf[..n]).map_err(|e| if cancel.is_cancelled() { Error::Cancelled } else { Error::io(dst, &e) })?;
            m.p.file_done += n as u64;
            m.p.bytes_done += n as u64;
            m.send(false);
        }
        w.finish()
    }
}

/// Where a walk is: how deep, whether at a selected object, whether its files are only copied (behind a
/// moved link to a folder).
#[derive(Clone, Copy)]
struct Walk {
    depth: usize,
    top: bool,
    keep: bool,
}

static ASIDE: AtomicU64 = AtomicU64::new(0);

/// A folder's entries; if it can't be read, the user is asked like for a failed file. `None` — skipped.
fn list_or_ask(src: &dyn Source, dir: &VPath, policy: &mut Policy, fb: &dyn Feedback, r: &mut Report, cancel: &Cancel) -> Result<Option<Vec<Entry>>> {
    let mut got = None;
    let out = retry(fb, policy, r, dir, false, &mut || {
        got = Some(src.list(dir, cancel)?);
        Ok(())
    })?;
    Ok(if out == Outcome::Done { got } else { None })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    Skipped,
    /// `Unsupported` where the caller has another way.
    Fallback,
}

/// Run `f`; on an error ask the user (or follow "skip all"). Shared by all operations.
fn retry(fb: &dyn Feedback, policy: &mut Policy, r: &mut Report, path: &VPath, fallback: bool, f: &mut dyn FnMut() -> Result<()>) -> Result<Outcome> {
    loop {
        match f() {
            Ok(()) => return Ok(Outcome::Done),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(Error::Unsupported) if fallback => return Ok(Outcome::Fallback),
            Err(e) => {
                let answer = if policy.skip_errors { Answer::Skip } else { fb.ask(Question::Failed { path: path.clone(), error: e.to_string() }) };
                match answer {
                    Answer::Skip | Answer::SkipAll => {
                        policy.skip_errors |= answer == Answer::SkipAll;
                        r.failed += 1;
                        r.errors.push(e.to_string());
                        return Ok(Outcome::Skipped);
                    }
                    Answer::Cancel => return Err(Error::Cancelled),
                    _ => {}
                }
            }
        }
    }
}

/// Delete `names` in `dir`, folders with everything inside. A link is removed itself, never followed.
pub fn delete(src: &dyn Source, dir: &VPath, names: &[String], fb: &dyn Feedback, cancel: &Cancel) -> Report {
    cancel.scoped(|| delete_scoped(src, dir, names, fb, cancel))
}

fn delete_scoped(src: &dyn Source, dir: &VPath, names: &[String], fb: &dyn Feedback, cancel: &Cancel) -> Report {
    let mut r = Report::default();
    let mut m = Meter { fb, p: Progress { planning: true, ..Progress::default() }, sent: None };
    m.send(true);
    let res = (|| -> Result<()> {
        // Plan: contents before their folder.
        let mut order: Vec<(VPath, bool)> = Vec::new();
        let mut policy = Policy::default();
        for n in names {
            let p = dir.child(n);
            match src.stat(&p) {
                Ok(e) => collect(src, &p, &e, 0, &mut order, &mut policy, &mut m, &mut r, cancel)?,
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(e) => {
                    r.failed += 1;
                    r.errors.push(e.to_string());
                }
            }
        }
        m.p.planning = false;
        m.p.files_total = order.len() as u64;
        m.send(true);
        let mut i = 0;
        while i < order.len() {
            cancel.check()?;
            // Files in a row go to the source together (S3 deletes them in batches); what fails in a batch
            // comes back below one by one, with the usual question.
            let run = order[i..].iter().take(BATCH).take_while(|(_, d)| !d).count();
            if run > 1 {
                let paths: Vec<VPath> = order[i..i + run].iter().map(|(p, _)| p.clone()).collect();
                m.p.current = paths[0].to_string();
                m.send(false);
                let results = src.remove_files(&paths, cancel);
                let mut again = Vec::new();
                for (p, res) in paths.into_iter().zip(results) {
                    match res {
                        Ok(()) => {
                            r.done += 1;
                            m.p.files_done += 1;
                        }
                        Err(Error::Cancelled) => return Err(Error::Cancelled),
                        Err(_) => again.push(p),
                    }
                }
                m.send(false);
                for p in again {
                    delete_one(src, &p, false, &mut policy, &mut m, &mut r, cancel)?;
                }
                i += run;
                continue;
            }
            let (p, is_dir) = order[i].clone();
            i += 1;
            delete_one(src, &p, is_dir, &mut policy, &mut m, &mut r, cancel)?;
        }
        m.send(true);
        Ok(())
    })();
    match res {
        Ok(()) => {}
        Err(Error::Cancelled) => r.cancelled = true,
        Err(e) => {
            r.failed += 1;
            r.errors.push(e.to_string());
        }
    }
    r
}

/// Files deleted together at most (S3 takes 1 000 keys a request).
const BATCH: usize = 1000;

/// One object of a delete: removed, or the question on failure; a folder that kept something is skipped.
#[allow(clippy::too_many_arguments)]
fn delete_one(src: &dyn Source, p: &VPath, is_dir: bool, policy: &mut Policy, m: &mut Meter, r: &mut Report, cancel: &Cancel) -> Result<()> {
    cancel.check()?;
    m.p.current = p.to_string();
    m.send(false);
    // A folder keeps what was skipped inside it: not an error of its own.
    let mut kept = false;
    let fb = m.fb;
    let out = retry(fb, policy, r, p, false, &mut || match src.remove(p) {
        Err(Error::DirNotEmpty(_)) if is_dir => {
            kept = true;
            Ok(())
        }
        x => x,
    })?;
    if kept {
        r.skipped += 1;
    } else if out == Outcome::Done {
        r.done += 1;
    }
    m.p.files_done += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect(
    src: &dyn Source,
    p: &VPath,
    e: &Entry,
    depth: usize,
    out: &mut Vec<(VPath, bool)>,
    policy: &mut Policy,
    m: &mut Meter,
    r: &mut Report,
    cancel: &Cancel,
) -> Result<()> {
    cancel.check()?;
    if e.kind == Kind::Dir {
        if depth > MAX_DEPTH {
            return Err(Error::msg(p, "folders nested too deep"));
        }
        m.p.current = p.to_string();
        m.send(false);
        // Unreadable and skipped: it stays, with everything in it.
        let Some(children) = list_or_ask(src, p, policy, m.fb, r, cancel)? else { return Ok(()) };
        for c in children {
            collect(src, &p.child(&c.name), &c, depth + 1, out, policy, m, r, cancel)?;
        }
        out.push((p.clone(), true));
    } else {
        out.push((p.clone(), false));
    }
    Ok(())
}

/// Make `rel` inside `dir`, all levels (`a/b/c`, as in TC). Returns the first level — where the panel cursor goes.
pub fn mkdir(src: &dyn Source, dir: &VPath, rel: &str) -> Result<VPath> {
    let rel = rel.trim();
    let target = dir.join(rel);
    if rel.is_empty() || !target.starts_with(dir) || target == *dir {
        return Err(Error::msg(dir, format!("bad folder name '{rel}'")));
    }
    let mut p = dir.clone();
    let mut first = None;
    for c in target.strip_prefix(dir).unwrap_or("").split('/') {
        p = p.child(c);
        first.get_or_insert_with(|| p.clone());
        match src.mkdir(&p) {
            Ok(()) => {}
            // An existing level is fine unless it is the last one.
            Err(Error::AlreadyExists(_)) if p != target => {}
            Err(e) => return Err(e),
        }
    }
    Ok(first.unwrap_or(target))
}

/// Shift+F4: an empty file `name` in `dir`. An existing one is left alone (`AlreadyExists`).
pub fn new_file(src: &dyn Source, dir: &VPath, name: &str) -> Result<VPath> {
    let name = name.trim();
    if !vfs::valid_name(name) {
        return Err(Error::msg(dir, format!("bad file name '{name}'")));
    }
    let path = dir.child(name);
    src.create(&path, &CreateOpts::default())?.finish()?;
    Ok(path)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
}

/// What a folder holds (Space on a folder in the panel). Links to folders are not followed.
pub fn dir_size(src: &dyn Source, dir: &VPath, cancel: &Cancel) -> Result<Size> {
    cancel.scoped(|| dir_size_scoped(src, dir, cancel))
}

fn dir_size_scoped(src: &dyn Source, dir: &VPath, cancel: &Cancel) -> Result<Size> {
    fn walk(src: &dyn Source, dir: &VPath, depth: usize, s: &mut Size, cancel: &Cancel) -> Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        for e in src.list(dir, cancel)? {
            cancel.check()?;
            if e.kind == Kind::Dir {
                s.dirs += 1;
                // An unreadable subfolder doesn't spoil the total.
                let _ = walk(src, &dir.child(&e.name), depth + 1, s, cancel);
                cancel.check()?;
            } else {
                s.files += 1;
                s.bytes += e.size.unwrap_or(0);
            }
        }
        Ok(())
    }
    let mut s = Size::default();
    walk(src, dir, 0, &mut s, cancel)?;
    Ok(s)
}
