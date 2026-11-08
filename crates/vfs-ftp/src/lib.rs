//! Files on an FTP server as a vfs source: plain FTP, FTPS with `AUTH TLS` (explicit) or TLS from the start
//! (implicit, port 990).
//!
//! A folder is `MLSD` where the server has it, `LIST` otherwise (Unix `ls -l` or DOS); a file is `RETR` from any
//! offset (`REST`). A file is written next to its place under a temporary name (`STOR`) and renamed over it at
//! `finish` (`RNFR`/`RNTO`): an unfinished write leaves nothing and spoils nothing. Times are set with `MFMT`,
//! or `MDTM` with a time (vsftpd); permissions with `SITE CHMOD`.
//!
//! A control connection carries one transfer at a time, so each call takes a connection of its own from a pool
//! (a file being read holds one until it is closed). The client is blocking, on `std::net` and rustls: sources
//! are called from workers only, and every wait stops on the worker's `Cancel`.

mod conn;
mod list;

use std::io::{self, Read, Write};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub use conn::Security;
use conn::{Conn, Data, Fail, Reply, Tls};
use vfs::{Cancel, Capabilities, CreateOpts, Entry, Error, Kind, Result, Root, Source, SourceInfo, VPath};

/// Connections kept open for the next call.
const IDLE: usize = 4;

#[derive(Clone, Default)]
pub struct Config {
    pub host: String,
    /// 0 — the usual one: 990 for implicit TLS, 21 otherwise.
    pub port: u16,
    /// Empty — `anonymous`.
    pub user: String,
    pub password: String,
    pub security: Security,
    /// Fingerprints (`SHA256:…`) of certificates to accept although the system doesn't: the user said yes.
    pub trusted: Vec<String>,
}

/// Without the password: a `{:?}` somewhere must not print it.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("security", &self.security)
            .finish_non_exhaustive()
    }
}

impl Config {
    pub fn port(&self) -> u16 {
        match (self.port, self.security) {
            (0, Security::Implicit) => 990,
            (0, _) => 21,
            (p, _) => p,
        }
    }
}

/// In the error of a certificate the system doesn't trust, before its fingerprint (which ends the message).
pub const UNTRUSTED: &str = "certificate not trusted, fingerprint ";

/// The fingerprint of a certificate the user may trust (an error of `FtpFs::connect`), and why the system
/// didn't.
pub fn untrusted_certificate(e: &Error) -> Option<(String, String)> {
    let s = e.to_string();
    let i = s.find(UNTRUSTED)?;
    let fp = s[i + UNTRUSTED.len()..].split_whitespace().next()?;
    Some((fp.to_string(), s[..i].trim_end_matches([':', ' ', ';']).to_string()))
}

/// `user@host:port`, `host`, `ftp://host:2121/` → (user, host, port); an IPv6 address in brackets.
pub fn split_server(s: &str) -> std::result::Result<(Option<String>, String, Option<u16>), String> {
    let t = s.trim();
    let t = ["ftp://", "ftps://", "ftpes://"].iter().find_map(|p| t.strip_prefix(p)).unwrap_or(t);
    let t = t.split('/').next().unwrap_or("");
    let (user, hostport) = match t.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()).filter(|u| !u.is_empty()), h),
        None => (None, t),
    };
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(|| format!("\"{s}\": no ] after the address"))?;
        (h, after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            _ => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err(format!("\"{s}\": no server: type host or host:port"));
    }
    let port = match port {
        Some(p) => Some(p.parse::<u16>().map_err(|_| format!("\"{s}\": the port is a number up to 65535"))?),
        None => None,
    };
    Ok((user, host.to_string(), port))
}

/// What the server says it can (`FEAT`, `SYST`).
#[derive(Clone, Copy, Debug, Default)]
struct Feat {
    mlsd: bool,
    mfmt: bool,
    mdtm: bool,
    utf8: bool,
    /// `SYST` says Unix: `SITE CHMOD`, names differ by case.
    unix: bool,
    windows: bool,
}

impl Feat {
    fn parse(feat: &str) -> Feat {
        let mut f = Feat::default();
        for l in feat.lines() {
            let w = l.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
            match w.as_str() {
                // MLST implies MLSD (RFC 3659).
                "MLST" | "MLSD" => f.mlsd = true,
                "MFMT" => f.mfmt = true,
                "MDTM" => f.mdtm = true,
                "UTF8" => f.utf8 = true,
                _ => {}
            }
        }
        f
    }
}

struct Pool {
    idle: Vec<Conn>,
    /// Connections out of the pool now.
    busy: usize,
}

struct Inner {
    cfg: Config,
    addr: SocketAddr,
    tls: Option<Arc<Tls>>,
    feat: Feat,
    /// `LIST -a` was refused (IIS takes `-a` for a name): plain `LIST`.
    no_list_a: AtomicBool,
    pool: Mutex<Pool>,
    freed: Condvar,
    /// `host:port`, for messages.
    host: String,
}

impl Inner {
    /// A new connection, signed in and set up.
    fn open(&self) -> std::result::Result<Conn, Fail> {
        let mut c = Conn::open(self.addr, self.cfg.security, self.tls.as_ref())?;
        login(&mut c, &self.cfg)?;
        if self.feat.utf8 {
            // Some servers are always UTF-8 and refuse the option: no matter.
            c.cmd("OPTS UTF8 ON")?;
        }
        c.expect("TYPE I", 200)?;
        Ok(c)
    }

    /// A connection from the pool (`true`: it was idle, maybe closed by the server since), or a new one.
    /// The server refusing one more while ours are busy: wait for one of them.
    fn take(self: &Arc<Self>) -> std::result::Result<(Lease, bool), Fail> {
        let deadline = Instant::now() + conn::ANSWER_TIMEOUT;
        loop {
            {
                let mut p = self.pool.lock().unwrap();
                p.busy += 1;
                if let Some(c) = p.idle.pop() {
                    return Ok((Lease { conn: Some(c), inner: self.clone() }, true));
                }
            }
            match self.open() {
                Ok(c) => return Ok((Lease { conn: Some(c), inner: self.clone() }, false)),
                Err(e) => {
                    let mut p = self.pool.lock().unwrap();
                    p.busy -= 1;
                    let too_many = matches!(&e, Fail::Lost(m) if m.contains("421") || m.to_lowercase().contains("too many"));
                    if !too_many || p.busy == 0 || Instant::now() > deadline {
                        return Err(e);
                    }
                    let (_p, _) = self.freed.wait_timeout(p, Duration::from_millis(200)).unwrap();
                    if Cancel::current().is_cancelled() {
                        return Err(Fail::Cancelled);
                    }
                }
            }
        }
    }

    fn give_back(&self, c: Conn) {
        let mut p = self.pool.lock().unwrap();
        p.busy -= 1;
        if !c.broken && p.idle.len() < IDLE {
            p.idle.push(c);
        }
        drop(p);
        self.freed.notify_all();
    }

    fn err(&self, path: &VPath, f: Fail) -> Error {
        match f {
            Fail::Cancelled => Error::Cancelled,
            Fail::Lost(m) => Error::Io { path: Some(path.clone()), msg: format!("{}: {m}", self.host) },
            Fail::Untrusted { fingerprint, why } => Error::Io { path: Some(path.clone()), msg: format!("{why}; {UNTRUSTED}{fingerprint}") },
        }
    }
}

/// A connection out of the pool: back into it when dropped, unless it broke.
struct Lease {
    conn: Option<Conn>,
    inner: Arc<Inner>,
}

impl std::ops::Deref for Lease {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        self.conn.as_ref().expect("conn")
    }
}

impl std::ops::DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut Conn {
        self.conn.as_mut().expect("conn")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.inner.give_back(c);
        }
    }
}

fn login(c: &mut Conn, cfg: &Config) -> std::result::Result<(), Fail> {
    let user = if cfg.user.is_empty() { "anonymous" } else { cfg.user.as_str() };
    let r = c.cmd(&format!("USER {user}"))?;
    let r = match r.code {
        331 | 332 => c.cmd(&format!("PASS {}", if cfg.user.is_empty() && cfg.password.is_empty() { "anonymous@" } else { &cfg.password }))?,
        _ => r,
    };
    match r.code {
        230 | 202 => Ok(()),
        530 if r.text.to_lowercase().contains("tls") || r.text.to_lowercase().contains("encrypt") || r.text.to_lowercase().contains("ssl") => {
            Err(Fail::Lost(format!("the server wants encryption ({r}): choose FTPS")))
        }
        530 => Err(Fail::Lost(format!("the server did not accept the user name or password ({})", r.text))),
        _ => Err(Fail::Lost(format!("sign-in: {r}"))),
    }
}

pub struct FtpFs {
    inner: Arc<Inner>,
    home: VPath,
}

static TEMP: AtomicU64 = AtomicU64::new(0);

impl FtpFs {
    /// Connects and signs in: a wrong password, an untrusted certificate or a server that is not there fail
    /// here, not at the first listing.
    pub fn connect(cfg: Config) -> Result<FtpFs> {
        let port = cfg.port();
        let host = format!("{}:{port}", cfg.host);
        let io = |m: String| Error::Io { path: None, msg: format!("{host}: {m}") };
        let addr = (cfg.host.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| io(e.to_string()))?
            .next()
            .ok_or_else(|| io("no address for this name".into()))?;
        let tls = match cfg.security {
            Security::None => None,
            _ => Some(Arc::new(Tls::new(&cfg.host, cfg.trusted.clone()).map_err(io)?)),
        };
        let mut inner = Inner {
            cfg,
            addr,
            tls,
            feat: Feat::default(),
            no_list_a: AtomicBool::new(false),
            pool: Mutex::new(Pool { idle: Vec::new(), busy: 0 }),
            freed: Condvar::new(),
            host,
        };
        let root = VPath::root();
        let first = (|| -> std::result::Result<(Conn, Feat, String), Fail> {
            let mut c = Conn::open(inner.addr, inner.cfg.security, inner.tls.as_ref())?;
            login(&mut c, &inner.cfg)?;
            let feat = c.cmd("FEAT")?;
            let mut f = if feat.ok() { Feat::parse(&feat.text) } else { Feat::default() };
            let syst = c.cmd("SYST")?;
            let syst = syst.text.to_ascii_uppercase();
            f.unix = syst.contains("UNIX");
            f.windows = syst.contains("WINDOWS");
            if f.utf8 {
                c.cmd("OPTS UTF8 ON")?;
            }
            c.expect("TYPE I", 200)?;
            let pwd = c.cmd("PWD")?;
            Ok((c, f, pwd_path(&pwd).unwrap_or_else(|| "/".into())))
        })();
        let (c, feat, home) = first.map_err(|f| match inner.err(&root, f) {
            Error::Io { msg, .. } => Error::Io { path: None, msg },
            e => e,
        })?;
        inner.feat = feat;
        let fs = FtpFs { inner: Arc::new(inner), home: VPath::new(&home) };
        fs.inner.pool.lock().unwrap().idle.push(c);
        Ok(fs)
    }

    /// `f` on a connection of the pool; one that turns out closed (an idle one the server dropped) is replaced
    /// by a new one and `f` runs again. The connection is handed back too, for a transfer that goes on.
    fn with_lease<T>(&self, path: &VPath, mut f: impl FnMut(&mut Conn) -> std::result::Result<T, Fail>) -> Result<(T, Lease)> {
        let (mut lease, mut idle) = self.inner.take().map_err(|e| self.inner.err(path, e))?;
        loop {
            match f(&mut lease) {
                Ok(v) => return Ok((v, lease)),
                Err(Fail::Lost(_)) if idle => {
                    drop(lease);
                    let (l, i) = self.inner.take().map_err(|e| self.inner.err(path, e))?;
                    (lease, idle) = (l, i);
                }
                Err(e) => return Err(self.inner.err(path, e)),
            }
        }
    }

    fn run<T>(&self, path: &VPath, f: impl FnMut(&mut Conn) -> std::result::Result<T, Fail>) -> Result<T> {
        self.with_lease(path, f).map(|(v, _)| v)
    }

    /// One command on `path`, its reply.
    fn cmd(&self, path: &VPath, verb: &str) -> Result<Reply> {
        let line = format!("{verb} {}", path.as_str());
        self.run(path, |c| c.cmd(&line))
    }

    fn exists(&self, path: &VPath) -> Result<bool> {
        match self.stat(path) {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// What a refusal about `path` means. FTP says 550 for "not there", "not allowed" and "already there"
    /// alike, so the path is looked at: missing — `NotFound` (its folder's, if that is missing too).
    fn why(&self, path: &VPath, r: &Reply) -> Error {
        let io = || Error::Io { path: Some(path.clone()), msg: r.to_string() };
        match r.code {
            552 => Error::Io { path: Some(path.clone()), msg: format!("no space left on the server ({})", r.text) },
            450 | 451 | 550 | 553 | 521 => match self.stat(path) {
                Err(Error::NotFound(_)) => match path.parent() {
                    Some(p) if !p.is_root() && matches!(self.stat(&p), Err(Error::NotFound(_))) => Error::NotFound(p),
                    _ => Error::NotFound(path.clone()),
                },
                Ok(_) if denied(&r.text) => Error::PermissionDenied(path.clone()),
                _ => io(),
            },
            _ => io(),
        }
    }

    /// The entries of `dir` as the server lists them.
    fn read_dir(&self, dir: &VPath) -> Result<std::result::Result<Vec<Entry>, Reply>> {
        let mlsd = self.inner.feat.mlsd;
        let no_a = &self.inner.no_list_a;
        let cwd = format!("CWD {}", dir.as_str());
        let lines = self.run(dir, |c| {
            let r = c.cmd(&cwd)?;
            if !r.ok() {
                return Ok(Err(r));
            }
            if mlsd {
                return c.lines("MLSD");
            }
            if !no_a.load(Ordering::Relaxed) {
                match c.lines("LIST -a")? {
                    Err(r) if r.code >= 500 => no_a.store(true, Ordering::Relaxed),
                    other => return Ok(other),
                }
            }
            c.lines("LIST")
        })?;
        let now = SystemTime::now();
        Ok(lines.map(|ls| ls.iter().filter_map(|l| if mlsd { list::mlsx(l) } else { list::list_line(l, now) }).collect()))
    }

    /// Links in a `LIST`: which lead to folders (`CWD` into them works).
    fn resolve_links(&self, dir: &VPath, entries: &mut [Entry]) -> Result<()> {
        for e in entries.iter_mut() {
            if let Kind::Symlink { to_dir, .. } = &mut e.kind {
                let p = dir.child(&e.name);
                let r = self.cmd(&p, "CWD")?;
                *to_dir = r.ok();
            }
        }
        Ok(())
    }
}

/// `257 "/home/me" is the current directory` → `/home/me` (`""` inside stands for `"`).
fn pwd_path(r: &Reply) -> Option<String> {
    if r.code != 257 {
        return None;
    }
    let t = &r.text;
    let start = t.find('"')? + 1;
    let mut out = String::new();
    let mut chars = t[start..].chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            if chars.peek() == Some(&'"') {
                chars.next();
                out.push('"');
                continue;
            }
            return Some(out).filter(|p| p.starts_with('/'));
        }
        out.push(c);
    }
    None
}

fn denied(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("denied") || t.contains("permission") || t.contains("not allowed")
}

impl Source for FtpFs {
    fn info(&self) -> SourceInfo {
        let c = &self.inner.cfg;
        let scheme = if c.security == Security::None { "ftp" } else { "ftps" };
        let user = if c.user.is_empty() { String::new() } else { format!("{}@", c.user) };
        let port = if c.port() == 21 || (c.port() == 990 && c.security == Security::Implicit) { String::new() } else { format!(":{}", c.port()) };
        let f = self.inner.feat;
        SourceInfo {
            id: format!("{scheme}://{user}{}{port}", c.host),
            title: format!("{scheme}:{}{port}", c.host),
            caps: Capabilities {
                write: true,
                rename: true,
                server_copy: false,
                real_dirs: true,
                seek: true,
                permissions: f.unix,
                set_mtime: f.mfmt || f.mdtm,
                case_sensitive: !f.windows,
            },
        }
    }

    fn roots(&self, _cancel: &Cancel) -> Result<Vec<Root>> {
        Ok(vec![Root { path: VPath::root(), title: "/".into() }])
    }

    fn home(&self) -> VPath {
        self.home.clone()
    }

    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>> {
        cancel.check()?;
        cancel.scoped(|| {
            let mut entries = match self.read_dir(dir)? {
                Ok(e) => e,
                Err(r) => {
                    return Err(match self.stat(dir) {
                        Ok(e) if !e.is_dir() => Error::NotADirectory(dir.clone()),
                        _ => self.why(dir, &r),
                    });
                }
            };
            self.resolve_links(dir, &mut entries)?;
            Ok(entries)
        })
    }

    fn stat(&self, path: &VPath) -> Result<Entry> {
        if path.is_root() {
            return Ok(Entry::new("/", Kind::Dir));
        }
        if self.inner.feat.mlsd {
            let r = self.cmd(path, "MLST")?;
            if r.code == 250 {
                // `250-Begin` / ` type=file;…; /path` / `250 End`: the facts are on the line after the first.
                let mut e = r.text.lines().skip(1).find_map(|l| list::mlsx(l.trim_start())).ok_or_else(|| Error::msg(path, format!("MLST: {r}")))?;
                e.name = path.name().to_string();
                e.hidden = e.name.starts_with('.');
                return Ok(e);
            }
            return Err(if r.code == 550 || r.code == 450 { Error::NotFound(path.clone()) } else { Error::msg(path, format!("MLST: {r}")) });
        }
        // The folder's listing, and the exact time of a file (`LIST` has minutes at most).
        let parent = path.parent().unwrap_or_else(VPath::root);
        let entries = match self.read_dir(&parent)? {
            Ok(e) => e,
            Err(_) => return Err(Error::NotFound(path.clone())),
        };
        let mut e = entries.into_iter().find(|e| e.name == path.name()).ok_or_else(|| Error::NotFound(path.clone()))?;
        if let Kind::Symlink { to_dir, .. } = &mut e.kind {
            *to_dir = self.cmd(path, "CWD")?.ok();
        }
        if e.kind == Kind::File && self.inner.feat.mdtm {
            let r = self.cmd(path, "MDTM")?;
            if r.code == 213
                && let Some(t) = list::timeval(&r.text)
            {
                e.modified = Some(t);
            }
        }
        Ok(e)
    }

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let line = format!("RETR {}", path.as_str());
        let (r, lease) = self.with_lease(path, |c| c.transfer(&line, offset))?;
        match r {
            Ok(data) => Ok(Box::new(FtpReader { lease: Some(lease), data: Some(data), path: path.clone() })),
            Err(r) => {
                drop(lease);
                // At or past the end: some servers refuse the offset.
                if offset > 0
                    && let Ok(e) = self.stat(path)
                    && e.kind == Kind::File
                    && e.size.is_some_and(|n| n <= offset)
                {
                    return Ok(Box::new(io::empty()));
                }
                Err(match self.stat(path) {
                    Ok(e) if e.is_dir() => Error::IsADirectory(path.clone()),
                    _ => self.why(path, &r),
                })
            }
        }
    }

    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn vfs::Writer>> {
        if !opts.overwrite && self.exists(path)? {
            return Err(Error::AlreadyExists(path.clone()));
        }
        let parent = path.parent().unwrap_or_else(VPath::root);
        let temp = parent.child(&format!(".uc-{}-{}.tmp", std::process::id(), TEMP.fetch_add(1, Ordering::Relaxed)));
        let line = format!("STOR {}", temp.as_str());
        let (r, lease) = self.with_lease(path, |c| c.transfer(&line, 0))?;
        match r {
            Ok(data) => Ok(Box::new(FtpWriter {
                fs: FtpFs { inner: self.inner.clone(), home: self.home.clone() },
                lease: Some(lease),
                data: Some(data),
                path: path.clone(),
                temp,
                opts: opts.clone(),
            })),
            Err(r) => {
                drop(lease);
                Err(match self.stat(&parent) {
                    Err(Error::NotFound(_)) => Error::NotFound(parent),
                    Ok(e) if !e.is_dir() => Error::NotADirectory(parent),
                    _ if denied(&r.text) => Error::PermissionDenied(path.clone()),
                    _ => Error::msg(path, format!("STOR: {r}")),
                })
            }
        }
    }

    fn mkdir(&self, path: &VPath) -> Result<()> {
        let r = self.cmd(path, "MKD")?;
        if r.code == 257 || r.ok() {
            return Ok(());
        }
        if self.exists(path)? {
            return Err(Error::AlreadyExists(path.clone()));
        }
        Err(self.why(path, &r))
    }

    fn remove(&self, path: &VPath) -> Result<()> {
        let r = self.cmd(path, "DELE")?;
        if r.ok() {
            return Ok(());
        }
        // Not a file (or not there): a folder is removed with RMD, and only when empty.
        let e = self.stat(path)?;
        if !e.is_dir() || matches!(e.kind, Kind::Symlink { .. }) {
            return Err(self.why(path, &r));
        }
        let r = self.cmd(path, "RMD")?;
        if r.ok() {
            return Ok(());
        }
        if !self.list(path, &Cancel::current())?.is_empty() {
            return Err(Error::DirNotEmpty(path.clone()));
        }
        Err(self.why(path, &r))
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        // RNTO replaces a file on most servers: the contract never does. A change of case only is not
        // "there already" (a server that ignores case finds the source by the new name).
        let case_only = from.as_str().to_lowercase() == to.as_str().to_lowercase();
        if !case_only && self.exists(to)? {
            return Err(Error::AlreadyExists(to.clone()));
        }
        self.move_over(from, to)
    }

    fn set_mtime(&self, path: &VPath, t: SystemTime) -> Result<()> {
        let f = self.inner.feat;
        if !f.mfmt && !f.mdtm {
            return Err(Error::Unsupported);
        }
        // MFMT (RFC draft, most servers), or MDTM with a time first (vsftpd).
        let verb = if f.mfmt { "MFMT" } else { "MDTM" };
        let r = self.cmd(path, &format!("{verb} {}", list::timeval_of(t)))?;
        match r.code {
            213 | 200 | 250 => Ok(()),
            500 | 501 | 502 | 504 => Err(Error::Unsupported),
            _ => Err(self.why(path, &r)),
        }
    }

    fn set_mode(&self, path: &VPath, mode: u32) -> Result<()> {
        let r = self.cmd(path, &format!("SITE CHMOD {:o}", mode & 0o7777))?;
        match r.code {
            200 | 250 => Ok(()),
            500 | 502 | 504 => Err(Error::Unsupported),
            _ => Err(self.why(path, &r)),
        }
    }

    fn case_sensitive(&self, _dir: &VPath) -> bool {
        !self.inner.feat.windows
    }
}

impl FtpFs {
    /// RNFR + RNTO on one connection (the server pairs them).
    fn move_over(&self, from: &VPath, to: &VPath) -> Result<()> {
        let (rnfr, rnto) = (format!("RNFR {}", from.as_str()), format!("RNTO {}", to.as_str()));
        let r = self.run(from, |c| {
            let r = c.cmd(&rnfr)?;
            if r.code != 350 {
                return Ok(Err((true, r)));
            }
            let r = c.cmd(&rnto)?;
            Ok(if r.ok() { Ok(()) } else { Err((false, r)) })
        })?;
        match r {
            Ok(()) => Ok(()),
            Err((true, r)) => Err(self.why(from, &r)),
            Err((false, r)) => Err(match self.stat(to) {
                Ok(_) => Error::AlreadyExists(to.clone()),
                Err(_) => self.why(to, &r),
            }),
        }
    }
}

/// A file being read: the data connection of a RETR and the control connection it came by.
struct FtpReader {
    lease: Option<Lease>,
    data: Option<Data>,
    path: VPath,
}

impl Read for FtpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(data) = &mut self.data else { return Ok(0) };
        let n = data.read(buf).inspect_err(|_| {
            if let Some(l) = &mut self.lease {
                l.broken = true;
            }
        })?;
        if n > 0 || buf.is_empty() {
            return Ok(n);
        }
        // The end: the server's word that it was all.
        if let Some(d) = self.data.take() {
            d.close();
        }
        let mut lease = self.lease.take().expect("lease");
        let r = lease.done().map_err(conn::io_error)?;
        if !r.ok() {
            lease.broken = r.code == 426;
            return Err(io::Error::other(format!("{}: {r}", self.path)));
        }
        Ok(0)
    }
}

impl Drop for FtpReader {
    /// Closed before the end: the transfer is aborted, and the connection kept if that goes well.
    fn drop(&mut self) {
        if let (Some(data), Some(mut lease)) = (self.data.take(), self.lease.take()) {
            lease.abort(data);
        }
    }
}

/// A file being written under a temporary name; `finish` moves it into its place.
struct FtpWriter {
    fs: FtpFs,
    lease: Option<Lease>,
    data: Option<Data>,
    path: VPath,
    temp: VPath,
    opts: CreateOpts,
}

impl Write for FtpWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let data = self.data.as_mut().ok_or_else(|| io::Error::other("finished"))?;
        data.write(buf).inspect_err(|_| {
            if let Some(l) = &mut self.lease {
                l.broken = true;
            }
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl FtpWriter {
    fn upload_done(&mut self) -> Result<()> {
        let Some(data) = self.data.take() else { return Err(Error::msg(&self.path, "the upload was finished")) };
        data.close();
        let mut lease = self.lease.take().expect("lease");
        let r = lease.done().map_err(|f| self.fs.inner.err(&self.path, f))?;
        drop(lease);
        match r.code {
            226 | 250 => Ok(()),
            552 => Err(Error::msg(&self.path, format!("no space left on the server ({})", r.text))),
            _ => Err(Error::msg(&self.path, format!("STOR: {r}"))),
        }
    }
}

impl vfs::Writer for FtpWriter {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let done = self.upload_done().and_then(|()| {
            // Time and permissions where the server takes them; a server that doesn't fails no copy.
            if let Some(t) = self.opts.modified {
                let _ = self.fs.set_mtime(&self.temp, t);
            }
            if let Some(m) = self.opts.mode.filter(|_| self.fs.inner.feat.unix) {
                let _ = self.fs.set_mode(&self.temp, m);
            }
            match self.fs.move_over(&self.temp, &self.path) {
                // A server that doesn't rename over a file (Windows): the old one goes first.
                Err(Error::AlreadyExists(_)) if self.opts.overwrite => {
                    self.fs.remove(&self.path)?;
                    self.fs.move_over(&self.temp, &self.path)
                }
                other => other,
            }
        });
        if done.is_err() {
            let _ = self.fs.cmd(&self.temp, "DELE");
        }
        done
    }
}

impl Drop for FtpWriter {
    /// Dropped without `finish`: the upload is aborted and its temporary file goes.
    fn drop(&mut self) {
        if let (Some(data), Some(mut lease)) = (self.data.take(), self.lease.take()) {
            lease.abort(data);
            drop(lease);
            let _ = self.fs.cmd(&self.temp, "DELE");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers() {
        assert_eq!(split_server("u@h:2121").unwrap(), (Some("u".into()), "h".into(), Some(2121)));
        assert_eq!(split_server(" ftp://h/pub ").unwrap(), (None, "h".into(), None));
        assert_eq!(split_server("me@mail.com@h").unwrap(), (Some("me@mail.com".into()), "h".into(), None));
        assert_eq!(split_server("[::1]:21").unwrap(), (None, "::1".into(), Some(21)));
        assert_eq!(split_server("::1").unwrap(), (None, "::1".into(), None));
        assert!(split_server("h:port").is_err() && split_server("u@").is_err());
    }

    #[test]
    fn replies() {
        let r = |code, text: &str| Reply { code, text: text.into() };
        assert_eq!(pwd_path(&r(257, "\"/ftp/uison\" is the current directory")).as_deref(), Some("/ftp/uison"));
        assert_eq!(pwd_path(&r(257, "\"/a \"\"q\"\" b\" is current")).as_deref(), Some("/a \"q\" b"));
        assert_eq!(pwd_path(&r(257, "\"C:\\\" is current")), None);
        let f = Feat::parse("Extensions supported:\n UTF8\n MLST type*;size*;modify*;\n MFMT\n MDTM\nEnd.");
        assert!(f.mlsd && f.mfmt && f.mdtm && f.utf8);
        let f = Feat::parse("Features:\n EPSV\n MDTM\n PASV\n REST STREAM\n SIZE\n UTF8\nEnd");
        assert!(!f.mlsd && !f.mfmt && f.mdtm);
        let e = Error::Io { path: None, msg: format!("it has expired; {UNTRUSTED}SHA256:AB:CD") };
        assert_eq!(untrusted_certificate(&e), Some(("SHA256:AB:CD".into(), "it has expired".into())));
    }
}
