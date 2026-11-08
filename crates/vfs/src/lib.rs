//! The source contract. A panel shows a source, operations read and write through it; neither knows
//! whether it is the local disk, a server over SSH or an S3 bucket.
//!
//! Every call may take seconds (network), so sources are called only from worker threads, never from
//! the UI thread. A worker sets its `Cancel` as the thread's current one (`Cancel::scoped`): a source that
//! waits on the network stops on it in any call, also those without a `Cancel` parameter.
//!
//! Recursion (deleting a tree, copying a folder) lives in `ops`, not here: a source does one object at a
//! time, so progress and cancellation work the same everywhere.

use std::fmt;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

pub mod conformance;
mod mem;
mod path;

pub use mem::MemFs;
pub use path::{VPath, split_ext, valid_name};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// The target as stored in the link (may be relative); `to_dir` — it leads to a folder (the panel enters it).
    Symlink {
        target: Option<String>,
        to_dir: bool,
    },
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// `None` — unknown (a folder whose size nobody has counted).
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Unix permission bits, if the source knows them.
    pub mode: Option<u32>,
    pub hidden: bool,
    pub readonly: bool,
    /// Properties only this source knows (S3: storage class, ETag, visibility) — shown in Properties as they are.
    pub extra: Vec<(String, String)>,
}

impl Entry {
    pub fn new(name: impl Into<String>, kind: Kind) -> Entry {
        Entry { name: name.into(), kind, size: None, modified: None, mode: None, hidden: false, readonly: false, extra: Vec::new() }
    }

    /// A folder or a link to one: the panel enters it.
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir | Kind::Symlink { to_dir: true, .. })
    }
}

/// What a source can do; `ops` takes a fallback path for what it can't.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// `false` — read only: nothing can be copied or moved into it, deleted or created.
    pub write: bool,
    /// Atomic rename inside the source.
    pub rename: bool,
    /// Copying inside the source without moving data through us (S3 CopyObject, cp on the server).
    pub server_copy: bool,
    /// `false` — folders are virtual (S3 prefixes): an empty folder may vanish.
    pub real_dirs: bool,
    /// Reading from any offset (the viewer of large files).
    pub seek: bool,
    pub permissions: bool,
    /// The modification time can be kept when copying.
    pub set_mtime: bool,
    /// `A.txt` and `a.txt` are different files — usually; [`Source::case_sensitive`] knows it for a folder.
    pub case_sensitive: bool,
}

#[derive(Clone, Debug)]
pub struct SourceInfo {
    /// `local`, `mem`, later `sftp://user@host`, `s3://profile`. Two sources with one id are the same storage:
    /// a move between them is a rename.
    pub id: String,
    /// What the panel header and the source list show.
    pub title: String,
    pub caps: Capabilities,
}

/// A place to start from: `/` and mounted volumes, drive letters, buckets.
#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    pub path: VPath,
    pub title: String,
}

#[derive(Clone, Debug, Default)]
pub struct CreateOpts {
    /// Replace an existing file; otherwise `AlreadyExists`.
    pub overwrite: bool,
    /// Expected size — S3 picks single or multipart upload by it.
    pub size_hint: Option<u64>,
    /// The modification time to give the file at `finish`, where the source can (`caps.set_mtime`). Set here
    /// rather than by `set_mtime` afterwards: over a network that is round trips saved on every file.
    pub modified: Option<SystemTime>,
    /// Permission bits to give the file at `finish`, where the source can (`caps.permissions`).
    pub mode: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    NotFound(VPath),
    AlreadyExists(VPath),
    PermissionDenied(VPath),
    NotADirectory(VPath),
    IsADirectory(VPath),
    DirNotEmpty(VPath),
    Unsupported,
    Cancelled,
    /// Everything else: text for a person.
    Io {
        path: Option<VPath>,
        msg: String,
    },
}

impl Error {
    pub fn io(path: &VPath, e: &std::io::Error) -> Error {
        use std::io::ErrorKind as K;
        let p = path.clone();
        match e.kind() {
            K::NotFound => Error::NotFound(p),
            K::AlreadyExists => Error::AlreadyExists(p),
            K::PermissionDenied => Error::PermissionDenied(p),
            K::NotADirectory => Error::NotADirectory(p),
            K::IsADirectory => Error::IsADirectory(p),
            K::DirectoryNotEmpty => Error::DirNotEmpty(p),
            K::Unsupported | K::CrossesDevices => Error::Unsupported,
            _ => Error::Io { path: Some(p), msg: e.to_string() },
        }
    }

    pub fn msg(path: &VPath, msg: impl Into<String>) -> Error {
        Error::Io { path: Some(path.clone()), msg: msg.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(p) => write!(f, "{p}: not found"),
            Error::AlreadyExists(p) => write!(f, "{p}: already exists"),
            Error::PermissionDenied(p) => write!(f, "{p}: permission denied"),
            Error::NotADirectory(p) => write!(f, "{p}: not a folder"),
            Error::IsADirectory(p) => write!(f, "{p}: is a folder"),
            Error::DirNotEmpty(p) => write!(f, "{p}: folder is not empty"),
            Error::Unsupported => f.write_str("not supported by this source"),
            Error::Cancelled => f.write_str("cancelled"),
            Error::Io { path: Some(p), msg } => write!(f, "{p}: {msg}"),
            Error::Io { path: None, msg } => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Cancellation token: cheap to clone, checked between blocks of work.
#[derive(Clone, Default, Debug)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    /// Run `f` with `self` as the worker thread's current cancel: calls that take no `Cancel` (stat, read,
    /// write, remove…) stop on it too where a source waits on the network. An operation lives in one worker
    /// thread, so this reaches every call it makes without a parameter on each.
    pub fn scoped<R>(&self, f: impl FnOnce() -> R) -> R {
        let before = CURRENT.with(|c| c.replace(Some(self.clone())));
        struct Restore(Option<Cancel>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let prev = self.0.take();
                CURRENT.with(|c| *c.borrow_mut() = prev);
            }
        }
        let _restore = Restore(before);
        f()
    }

    /// The current cancel of this thread (`scoped`); outside of one, a cancel nobody sets.
    pub fn current() -> Cancel {
        CURRENT.with(|c| c.borrow().clone()).unwrap_or_default()
    }
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Cancel>> = const { std::cell::RefCell::new(None) };
}

/// A file being written. Others see the data only after `finish`; dropped without it, the write is
/// cancelled and no half-written file is left under the real name (S3: abort multipart).
pub trait Writer: Write + Send {
    fn finish(self: Box<Self>) -> Result<()>;
}

/// An extra action of a source over the selection (S3: "Visibility…", "Copy web link").
#[derive(Clone, Debug, PartialEq)]
pub struct ActionInfo {
    pub id: String,
    pub title: String,
    /// Applies to several selected objects at once.
    pub multi: bool,
    /// What to ask before running; empty — run at once.
    pub params: Vec<Param>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    /// Options are (value, title).
    Choice {
        id: String,
        title: String,
        options: Vec<(String, String)>,
        default: String,
    },
    Text {
        id: String,
        title: String,
        default: String,
    },
    Flag {
        id: String,
        title: String,
        default: bool,
    },
    /// Text that is not shown while typed: a password, a secret key.
    Secret {
        id: String,
        title: String,
    },
}

impl Param {
    pub fn id(&self) -> &str {
        match self {
            Param::Choice { id, .. } | Param::Text { id, .. } | Param::Flag { id, .. } | Param::Secret { id, .. } => id,
        }
    }
}

/// Answers to `params`: (id, value); a flag is "true" or "false".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionArgs(pub Vec<(String, String)>);

impl ActionArgs {
    pub fn get(&self, id: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == id).map(|(_, v)| v.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ActionResult {
    None,
    /// Read the panel again.
    Refresh,
    /// Put into the clipboard (a link; several — one per line).
    Clipboard(String),
    OpenUrl(String),
    Message(String),
}

/// A storage a panel can show. `Arc<dyn Source>`: two panels may look into one source while an operation
/// runs in it, so everything is `&self` and thread-safe.
pub trait Source: Send + Sync {
    fn info(&self) -> SourceInfo;
    /// Places to start from (Alt+F1 / Alt+F2).
    fn roots(&self, cancel: &Cancel) -> Result<Vec<Root>>;
    /// The starting folder.
    fn home(&self) -> VPath;

    /// The folder's entries, without `.` and `..`, in no particular order.
    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>>;
    fn stat(&self, path: &VPath) -> Result<Entry>;

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>>;
    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn Writer>>;

    fn mkdir(&self, path: &VPath) -> Result<()>;
    /// One file or an EMPTY folder.
    fn remove(&self, path: &VPath) -> Result<()>;
    /// Many files (not folders) at once, one result each, in order. A storage that deletes in batches (S3:
    /// 1 000 keys a request) makes a big delete a few requests instead of one per file; by default one by one.
    fn remove_files(&self, paths: &[VPath], cancel: &Cancel) -> Vec<Result<()>> {
        paths.iter().map(|p| cancel.check().and_then(|()| self.remove(p))).collect()
    }

    // Optional: `Unsupported` by default, and `ops` takes a fallback path.
    /// Never replaces an existing target: `AlreadyExists`.
    fn rename(&self, _from: &VPath, _to: &VPath) -> Result<()> {
        Err(Error::Unsupported)
    }
    /// Replaces an existing file at `to` (the user has been asked already). `Unsupported` for this pair
    /// (other volume, not a file) — `ops` streams the data instead.
    fn copy_within(&self, _from: &VPath, _to: &VPath, _cancel: &Cancel) -> Result<()> {
        Err(Error::Unsupported)
    }
    fn set_mtime(&self, _path: &VPath, _t: SystemTime) -> Result<()> {
        Err(Error::Unsupported)
    }
    fn set_mode(&self, _path: &VPath, _mode: u32) -> Result<()> {
        Err(Error::Unsupported)
    }
    /// Do names in `dir` differ by case? A source spanning volumes (the local disk: APFS, a case-sensitive APFS,
    /// exFAT) answers per folder; by default the capability.
    fn case_sensitive(&self, _dir: &VPath) -> bool {
        self.info().caps.case_sensitive
    }
    /// `a` and `b` are one object reached by two paths: a link in the path (`/tmp` and `/private/tmp`), a hard
    /// link, another case of the name. `ops` asks before overwriting a target, and overwriting a file with
    /// itself deletes it. By default only the same path.
    fn same_file(&self, a: &VPath, b: &VPath) -> bool {
        a == b
    }

    /// The file's path on this machine, for the system's own apps (open with, edit); `None` — not a file here.
    fn local_path(&self, _path: &VPath) -> Option<std::path::PathBuf> {
        None
    }
    /// Free and total bytes of the volume holding `dir`; `None` — the source has no such thing (S3).
    fn space(&self, _dir: &VPath) -> Option<(u64, u64)> {
        None
    }

    /// Extra actions for the selection in `dir`.
    fn actions(&self, _dir: &VPath, _sel: &[Entry]) -> Vec<ActionInfo> {
        Vec::new()
    }
    /// Before the parameters are asked: the action with what is there now as the defaults (S3: the current
    /// visibility). May go to the network, so it runs in a worker; by default the action as it is.
    fn prepare_action(&self, action: &ActionInfo, _sel: &[VPath], _cancel: &Cancel) -> Result<ActionInfo> {
        Ok(action.clone())
    }
    fn run_action(&self, _id: &str, _sel: &[VPath], _args: &ActionArgs, _cancel: &Cancel) -> Result<ActionResult> {
        Err(Error::Unsupported)
    }
}

/// The same storage: a move between the two is a rename, a copy may stay on the server.
pub fn same_storage(a: &dyn Source, b: &dyn Source) -> bool {
    a.info().id == b.info().id
}
