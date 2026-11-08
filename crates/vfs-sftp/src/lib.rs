//! Files on a server over SSH (SFTP) as a source. Paths are the server's own absolute paths; the start is
//! the home folder of the user, or the folder the user named (`Config::folder`): on a server where the
//! folders above it can't be read, it can't be reached by going down from `/` or the home.
//!
//! Sign-in: a password, a key file (with its passphrase), or — when neither is given — the keys of
//! ssh-agent and then `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`. The host key is checked against
//! `~/.ssh/known_hosts` and our own list (`Config::known_hosts`). A new host is refused with
//! [`UNKNOWN_HOST`] and its fingerprint, until the user trusts exactly that fingerprint (`Trust::Fingerprint`),
//! as `ssh` asks; a changed key is refused.
//!
//! russh is async; the contract is not (it is called from worker threads only). Every call runs on one
//! shared Tokio runtime with `block_on`. The protocol is spoken through `RawSftpSession`: reads and writes
//! keep several requests in flight (one at a time waits a round trip each), and an existing file is replaced
//! atomically with `posix-rename@openssh.com`. A connection that broke is made again on the next call.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use tokio::task::JoinHandle;
use vfs::{Cancel, Capabilities, CreateOpts, Entry, Error, Kind, Result, Root, Source, SourceInfo, VPath};

/// What the user typed to connect.
#[derive(Clone, Default)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Sign in with this password.
    pub password: Option<String>,
    /// Or with this key file (and its passphrase).
    pub key: Option<PathBuf>,
    pub passphrase: Option<String>,
    /// Our own list of known hosts, besides `~/.ssh/known_hosts`.
    pub known_hosts: PathBuf,
    /// What to do with a host that is in neither list.
    pub trust: Trust,
    /// Where to start instead of the home: absolute, or from the home (`www`, `~/www`). Empty — the home.
    pub folder: String,
}

/// A host key seen for the first time.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Trust {
    /// Refuse, saying [`UNKNOWN_HOST`] and the fingerprint: the user decides.
    #[default]
    Ask,
    /// Accept and remember the key if it has this fingerprint (what the user was shown).
    Fingerprint(String),
    /// Accept and remember any new key (tests against a fresh server).
    Any,
}

/// In the error of a connection refused for an unknown host: `UNKNOWN_HOST` then the fingerprint.
pub const UNKNOWN_HOST: &str = "unknown host key";

/// The fingerprint in an `UNKNOWN_HOST` error, if that is what it is.
pub fn unknown_host(e: &Error) -> Option<String> {
    let s = e.to_string();
    let i = s.find(UNKNOWN_HOST)?;
    s[i + UNKNOWN_HOST.len()..].split_whitespace().next().map(str::to_string)
}

/// Without the password and passphrase: a `{:?}` somewhere must not print them.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("key", &self.key)
            .field("trust", &self.trust)
            .field("folder", &self.folder)
            .finish_non_exhaustive()
    }
}

/// `[user@]host[:port]` (`[::1]:2222` for IPv6) → (user, host, port) as typed: what is not typed is `None`.
pub fn split_server(s: &str) -> std::result::Result<(Option<String>, String, Option<u16>), String> {
    let s = s.trim();
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, r)) => (Some(u.to_string()), r),
        None => (None, s),
    };
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or("an IPv6 address goes in brackets: [::1]:22")?;
        (h, after.strip_prefix(':'))
    } else {
        match rest.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (rest, None),
        }
    };
    let port = match port {
        Some(p) => Some(p.parse().map_err(|_| format!("\"{p}\" is not a port"))?),
        None => None,
    };
    if host.is_empty() {
        return Err("no server: type user@host or host".into());
    }
    if user.as_deref() == Some("") {
        return Err("no user: type user@host".into());
    }
    Ok((user, host.to_string(), port))
}

/// `[user@]host[:port]` → (user, host, port); the user defaults to `$USER`, the port to 22.
pub fn parse_server(s: &str) -> std::result::Result<(String, String, u16), String> {
    let (user, host, port) = split_server(s)?;
    let user = user.unwrap_or_else(|| std::env::var("USER").unwrap_or_default());
    if user.is_empty() {
        return Err("no user: type user@host".into());
    }
    Ok((user, host, port.unwrap_or(22)))
}

/// What `~/.ssh/config` says of a host name, as `ssh` reads it: the first value of each keyword from the
/// `Host` blocks that match, in file order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostConfig {
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// As written (`~/.ssh/id_work`).
    pub identity_file: Option<String>,
}

/// `~/.ssh/config` (its text) for `alias`. `Match` blocks and `Include` are not followed.
pub fn ssh_config(text: &str, alias: &str) -> HostConfig {
    let mut out = HostConfig::default();
    // Before any `Host` line, settings apply to every host.
    let mut on = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = match line.split_once(|c: char| c == '=' || c.is_whitespace()) {
            Some((k, v)) => (k.to_ascii_lowercase(), v.trim_start_matches(|c: char| c == '=' || c.is_whitespace()).trim()),
            None => continue,
        };
        let value = value.trim_matches('"');
        match key.as_str() {
            "host" => on = host_matches(value, alias),
            "match" => on = false,
            _ if !on => {}
            "hostname" => {
                out.host_name.get_or_insert_with(|| value.replace("%h", alias));
            }
            "user" => {
                out.user.get_or_insert_with(|| value.to_string());
            }
            "port" => {
                if out.port.is_none() {
                    out.port = value.parse().ok();
                }
            }
            "identityfile" => {
                out.identity_file.get_or_insert_with(|| value.to_string());
            }
            _ => {}
        }
    }
    out
}

/// The `Host` names of `~/.ssh/config` that are names, not patterns: the hosts to offer.
pub fn ssh_config_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some((k, v)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else { continue };
        if k.eq_ignore_ascii_case("host") {
            for h in v.split_whitespace() {
                if !h.contains(['*', '?', '!']) && !out.iter().any(|x| x == h) {
                    out.push(h.to_string());
                }
            }
        }
    }
    out
}

/// `Host a b*.lan !c`: any pattern matches and no negated one does.
fn host_matches(patterns: &str, host: &str) -> bool {
    let mut hit = false;
    for p in patterns.split_whitespace() {
        match p.strip_prefix('!') {
            Some(neg) if wildcard(neg, host) => return false,
            Some(_) => {}
            None => hit |= wildcard(p, host),
        }
    }
    hit
}

/// `*` and `?` (case-insensitive, as host names).
fn wildcard(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.to_lowercase().chars().collect(), s.to_lowercase().chars().collect());
    let (mut pi, mut si, mut star) = (0, 0, None);
    while si < s.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, si));
                pi += 1;
            }
            Some(&c) if c == '?' || c == s[si] => {
                pi += 1;
                si += 1;
            }
            _ => match star {
                Some((sp, ss)) => {
                    pi = sp + 1;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

use vfs_rt::{rt, run};

/// The most read or write requests in flight for one file (`UISON_SFTP_DEPTH` overrides, for measuring).
/// A read starts with one and doubles with every answer: the viewer reads a little, a copy to the end.
/// At 50 ms round trip a download gave 3.9 MB/s with 1, 40 with 16 and 57 with 64 (Task 2, experiment 1).
fn depth() -> usize {
    static D: OnceLock<usize> = OnceLock::new();
    *D.get_or_init(|| std::env::var("UISON_SFTP_DEPTH").ok().and_then(|v| v.parse().ok()).unwrap_or(64))
}

/// The SSH channel window we give the server.
const WINDOW: u32 = 16 << 20;

/// Bytes per request when the server doesn't say (`limits@openssh.com`): 32 KiB, which every server takes.
const CHUNK_DEFAULT: u32 = 32 << 10;
/// Bytes per request at most, whatever the server allows: a reply must fit russh-sftp's 256 KiB packet.
const CHUNK_MAX: u32 = 255 << 10;
/// One request without an answer this long: the connection is taken for dead.
const REQUEST_TIMEOUT: u64 = 30;
/// Connecting, signing in and opening SFTP together.
const OPEN_TIMEOUT: Duration = Duration::from_secs(45);

struct Conn {
    ssh: Handle<HostCheck>,
    raw: Arc<RawSftpSession>,
    /// Bytes per read or write request.
    chunk: u32,
    /// `posix-rename@openssh.com`: rename that replaces the target in one step.
    posix_rename: bool,
    /// `copy-data`: the server copies between two open files itself (OpenSSH 9.0+).
    copy_data: bool,
    /// `statvfs@openssh.com`: the free space of the server's volume.
    statvfs: bool,
}

pub struct SftpFs {
    cfg: Config,
    /// The server copies files itself (`copy-data`); known at connect, asked by `info` (the UI thread).
    copy_data: bool,
    conn: Mutex<Option<Arc<Conn>>>,
    /// Held while a new connection is made, so two workers don't both make one (the `conn` lock is not
    /// held that long: calls on a live connection go on).
    connecting: Mutex<()>,
    home: VPath,
    /// Where a panel opens: the home, or `Config::folder`.
    start: VPath,
}

/// `Config::folder` on the server: absolute as it is, otherwise from the home.
fn start(home: &VPath, folder: &str) -> VPath {
    let f = folder.trim();
    let f = if f == "~" { "" } else { f.strip_prefix("~/").unwrap_or(f) };
    home.join(f)
}

/// Checks the host key; says what it did (a new host, a changed key) through `note`.
struct HostCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    trust: Trust,
    note: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> std::result::Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.note.lock().unwrap() = Some("The server offers a certificate; only plain host keys are supported.".into());
            return Ok(false);
        };
        let fingerprint = key.fingerprint(Default::default());
        let system = russh::keys::check_known_hosts(&self.host, self.port, key);
        let ours = russh::keys::check_known_hosts_path(&self.host, self.port, key, &self.known_hosts);
        match (system, ours) {
            (Ok(true), _) | (_, Ok(true)) => Ok(true),
            (Err(russh::keys::Error::KeyChanged { .. }), _) | (_, Err(russh::keys::Error::KeyChanged { .. })) => {
                *self.note.lock().unwrap() = Some(format!(
                    "The host key of {} has CHANGED ({fingerprint}). Refused: if that is expected, remove the old key from known_hosts.",
                    self.host
                ));
                Ok(false)
            }
            _ => {
                let fp = fingerprint.to_string();
                let trusted = match &self.trust {
                    Trust::Any => true,
                    Trust::Fingerprint(f) => *f == fp,
                    Trust::Ask => false,
                };
                if !trusted {
                    *self.note.lock().unwrap() = Some(format!("{}: {UNKNOWN_HOST} {fp}", self.host));
                    return Ok(false);
                }
                let saved = russh::keys::known_hosts::learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts);
                *self.note.lock().unwrap() = Some(match saved {
                    Ok(()) => format!("First connection to {}: the server's identity ({fp}) is saved and checked next time.", self.host),
                    Err(e) => format!("Connected to {} ({fp}), but its identity could not be saved ({e}): it will be asked again.", self.host),
                });
                Ok(true)
            }
        }
    }
}

/// `~/.ssh/<name>`.
fn ssh_file(name: &str) -> Option<PathBuf> {
    std::env::home_dir().map(|h| h.join(".ssh").join(name))
}

type Agent = AgentClient<Box<dyn AgentStream + Send + Unpin>>;

/// The running ssh-agent: `SSH_AUTH_SOCK` on Unix; on Windows the OpenSSH agent's pipe, else Pageant.
async fn agent() -> Option<Agent> {
    #[cfg(unix)]
    return AgentClient::connect_env().await.ok().map(AgentClient::dynamic);
    #[cfg(windows)]
    {
        let pipe = std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".into());
        if let Ok(a) = AgentClient::connect_named_pipe(pipe).await {
            return Some(a.dynamic());
        }
        AgentClient::connect_pageant().await.ok().map(AgentClient::dynamic)
    }
    #[cfg(not(any(unix, windows)))]
    None
}

/// Sign in as the config says; the error tells what was tried.
async fn sign_in(s: &mut Handle<HostCheck>, cfg: &Config) -> std::result::Result<(), String> {
    let who = format!("{}@{}", cfg.user, cfg.host);
    let rsa = s.best_supported_rsa_hash().await.map_err(|e| e.to_string())?.flatten();
    if let Some(pw) = &cfg.password {
        let ok = s.authenticate_password(&cfg.user, pw).await.map_err(|e| e.to_string())?.success();
        return if ok { Ok(()) } else { Err(format!("{who}: the server did not accept the password.")) };
    }
    if let Some(path) = &cfg.key {
        let key = russh::keys::load_secret_key(path, cfg.passphrase.as_deref()).map_err(|e| format!("{}: {e}", path.display()))?;
        let ok = s.authenticate_publickey(&cfg.user, PrivateKeyWithHashAlg::new(Arc::new(key), rsa)).await.map_err(|e| e.to_string())?.success();
        return if ok { Ok(()) } else { Err(format!("{who}: the server did not accept the key {}.", path.display())) };
    }
    let mut tried = Vec::new();
    if let Some(mut agent) = agent().await {
        let ids = agent.request_identities().await.unwrap_or_default();
        if !ids.is_empty() {
            tried.push(format!("{} key(s) of ssh-agent", ids.len()));
        }
        for id in ids {
            let AgentIdentity::PublicKey { key, .. } = id else { continue };
            if s.authenticate_publickey_with(&cfg.user, key, rsa, &mut agent).await.is_ok_and(|r| r.success()) {
                return Ok(());
            }
        }
    }
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let Some(path) = ssh_file(name).filter(|p| p.exists()) else { continue };
        tried.push(format!("~/.ssh/{name}"));
        // A key with a passphrase is skipped here: it is typed in the dialog.
        let Ok(key) = russh::keys::load_secret_key(&path, None) else { continue };
        if s.authenticate_publickey(&cfg.user, PrivateKeyWithHashAlg::new(Arc::new(key), rsa)).await.is_ok_and(|r| r.success()) {
            return Ok(());
        }
    }
    Err(if tried.is_empty() {
        format!("{who}: no key to sign in with (no ssh-agent, no ~/.ssh/id_*): type a password or a key file.")
    } else {
        format!("{who}: the server accepted none of: {}. Type a password or a key file.", tried.join(", "))
    })
}

/// Connect, sign in, open SFTP. The note says what the host key check did, if anything.
async fn open(cfg: &Config) -> std::result::Result<(Conn, Option<String>), String> {
    let note = Arc::new(Mutex::new(None));
    let handler =
        HostCheck { host: cfg.host.clone(), port: cfg.port, known_hosts: cfg.known_hosts.clone(), trust: cfg.trust.clone(), note: note.clone() };
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        // What the server may send before we read it: with russh's 2 MB a download stopped at 15 MB/s on a
        // 50 ms link; 16 MB gives 57 MB/s (Task 2, experiment 1).
        window_size: WINDOW,
        ..Default::default()
    });
    let addr = (cfg.host.as_str(), cfg.port);
    let connected = tokio::time::timeout(Duration::from_secs(15), client::connect(config, addr, handler))
        .await
        .map_err(|_| format!("{}:{}: no answer in 15 s", cfg.host, cfg.port))?;
    let mut ssh = match connected {
        Ok(s) => s,
        // A refused host key says why itself.
        Err(e) => return Err(note.lock().unwrap().take().unwrap_or_else(|| format!("{}:{}: {e}", cfg.host, cfg.port))),
    };
    sign_in(&mut ssh, cfg).await?;
    let channel = ssh.channel_open_session().await.map_err(|e| e.to_string())?;
    channel.request_subsystem(true, "sftp").await.map_err(|e| e.to_string())?;
    let sftp_cfg = russh_sftp::client::Config { request_timeout_secs: REQUEST_TIMEOUT, ..Default::default() };
    let mut raw = RawSftpSession::new_with_config(channel.into_stream(), sftp_cfg);
    let no_sftp = |e: SftpError| format!("{}: no SFTP on this server ({e})", cfg.host);
    let version = raw.init().await.map_err(no_sftp)?;
    let has = |name: &str, ver: &str| version.extensions.get(name).is_some_and(|v| v == ver);
    let mut chunk = CHUNK_DEFAULT;
    if has("limits@openssh.com", "1") {
        let limits = Limits::from(raw.limits().await.map_err(no_sftp)?);
        let most = limits.read_len.unwrap_or(CHUNK_DEFAULT as u64).min(limits.write_len.unwrap_or(CHUNK_DEFAULT as u64));
        chunk = (most as u32).clamp(CHUNK_DEFAULT, CHUNK_MAX);
        raw.set_limits(limits);
    }
    let posix_rename = has("posix-rename@openssh.com", "1");
    let copy_data = has("copy-data", "1");
    let statvfs = has("statvfs@openssh.com", "2");
    let note = note.lock().unwrap().take();
    Ok((Conn { ssh, raw: Arc::new(raw), chunk, posix_rename, copy_data, statvfs }, note))
}

/// `open` with a limit on the whole: a hung ssh-agent (waiting for a touch) or a silent server can't hold
/// the worker forever.
fn open_blocking(cfg: &Config) -> Result<(Conn, Option<String>)> {
    run(async { tokio::time::timeout(OPEN_TIMEOUT, open(cfg)).await })
        .map_err(|_| format!("{}: no connection in {} s", cfg.host, OPEN_TIMEOUT.as_secs()))
        .and_then(|r| r)
        .map_err(|m| Error::Io { path: None, msg: m })
}

fn err(p: &VPath, e: SftpError) -> Error {
    match e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => Error::NotFound(p.clone()),
            StatusCode::PermissionDenied => Error::PermissionDenied(p.clone()),
            StatusCode::OpUnsupported => Error::Unsupported,
            c if s.error_message.is_empty() => Error::msg(p, c.to_string()),
            _ => Error::msg(p, s.error_message),
        },
        SftpError::Timeout => Error::msg(p, "no answer from the server"),
        e => Error::msg(p, e.to_string()),
    }
}

/// The connection itself is gone (not just this request refused).
fn broken(e: &SftpError) -> bool {
    matches!(e, SftpError::Timeout | SftpError::IO(_) | SftpError::UnexpectedBehavior(_))
}

fn status_is(e: &SftpError, code: StatusCode) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == code)
}

fn time(t: Option<u32>) -> Option<SystemTime> {
    t.map(|s| UNIX_EPOCH + Duration::from_secs(s.into()))
}

/// A temporary name next to the target: the file gets its real name only when written to the end.
fn tmp_near(p: &VPath) -> VPath {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    p.parent().unwrap_or_else(VPath::root).child(&format!(".uc-{}-{n}.tmp", std::process::id()))
}

/// The type of an object by the `S_IFMT` bits. Not russh-sftp's `is_dir` / `is_regular` / `is_symlink`: they
/// test single bits, and a link (0o120000) has the bits of a file (0o100000), a socket (0o140000) those of a
/// folder too — links showed as files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Type {
    Dir,
    File,
    Link,
    Other,
}

fn kind(a: &FileAttributes) -> Type {
    match a.permissions.unwrap_or(0) & 0o170000 {
        0o040000 => Type::Dir,
        0o100000 => Type::File,
        0o120000 => Type::Link,
        _ => Type::Other,
    }
}

/// An SSH string: length, then bytes.
fn ssh_string(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// An entry from what `lstat` (or a folder listing) says, and — for a link — what it points to.
fn entry(name: &str, a: &FileAttributes, link: Option<(Option<FileAttributes>, Option<String>)>) -> Entry {
    let mut e = Entry::new(name, Kind::Other);
    let (target, to) = link.unwrap_or((None, None));
    e.kind = if kind(a) == Type::Dir {
        Kind::Dir
    } else if kind(a) == Type::File {
        Kind::File
    } else if kind(a) == Type::Link {
        Kind::Symlink { target: to, to_dir: target.as_ref().is_some_and(|t| kind(t) == Type::Dir) }
    } else {
        Kind::Other
    };
    // A link shows what it points to: size and time of the target.
    let shown = target.as_ref().unwrap_or(a);
    if kind(shown) != Type::Dir {
        e.size = shown.size;
    }
    e.modified = time(shown.mtime);
    e.mode = shown.permissions.map(|m| m & 0o7777);
    e.readonly = e.mode.is_some_and(|m| m & 0o222 == 0);
    e.hidden = name.starts_with('.');
    if let (Some(u), Some(g)) = (&shown.user, &shown.group) {
        e.extra.push(("Owner".into(), format!("{u}:{g}")));
    } else if let (Some(u), Some(g)) = (shown.uid, shown.gid) {
        e.extra.push(("Owner".into(), format!("{u}:{g}")));
    }
    e
}

/// What a link points to and its text, both asked at once.
async fn follow(raw: Arc<RawSftpSession>, path: String) -> (Option<FileAttributes>, Option<String>) {
    let (target, to) = tokio::join!(raw.stat(path.clone()), raw.readlink(path));
    (target.ok().map(|a| a.attrs), to.ok().and_then(|n| n.files.into_iter().next()).map(|f| f.filename))
}

impl SftpFs {
    /// Connect and sign in. The note (a new host remembered) is for the user.
    pub fn connect(cfg: Config) -> Result<(SftpFs, Option<String>)> {
        let (conn, note) = open_blocking(&cfg)?;
        let home = run(conn.raw.realpath(".")).ok().and_then(|n| n.files.into_iter().next()).map_or_else(VPath::root, |f| VPath::new(&f.filename));
        let start = start(&home, &cfg.folder);
        let copy_data = conn.copy_data;
        let fs = SftpFs { cfg, copy_data, conn: Mutex::new(Some(Arc::new(conn))), connecting: Mutex::new(()), home, start };
        // A folder named must be there: a typo is said now, not as an empty panel.
        if fs.start != fs.home {
            match fs.stat(&fs.start)?.kind {
                Kind::Dir | Kind::Symlink { to_dir: true, .. } => {}
                _ => return Err(Error::NotADirectory(fs.start.clone())),
            }
        }
        Ok((fs, note))
    }

    /// The live connection; a broken one is made again.
    fn conn(&self) -> Result<Arc<Conn>> {
        let alive = |c: &Option<Arc<Conn>>| c.as_ref().filter(|c| !c.ssh.is_closed()).cloned();
        if let Some(c) = alive(&self.conn.lock().unwrap()) {
            return Ok(c);
        }
        let _one = self.connecting.lock().unwrap();
        // Someone else may have made it while we waited.
        if let Some(c) = alive(&self.conn.lock().unwrap()) {
            return Ok(c);
        }
        let (conn, _) = open_blocking(&self.cfg)?;
        let conn = Arc::new(conn);
        *self.conn.lock().unwrap() = Some(conn.clone());
        Ok(conn)
    }

    /// Forget a connection that failed, so the next call makes a new one.
    fn drop_conn(&self, c: &Arc<Conn>) {
        let mut cur = self.conn.lock().unwrap();
        if cur.as_ref().is_some_and(|x| Arc::ptr_eq(x, c)) {
            *cur = None;
        }
    }

    /// Run a request; if the connection turns out dead, forget it, and — for a request that changes nothing
    /// (`again`) — try once more on a new one.
    fn call<T, F, Fut>(&self, again: bool, f: F) -> Result<std::result::Result<T, SftpError>>
    where
        F: Fn(Arc<Conn>) -> Fut,
        Fut: Future<Output = std::result::Result<T, SftpError>>,
    {
        // The worker's current cancel (`Cancel::scoped`): Esc stops a request waiting on a slow link.
        self.call_cancel(again, Some(&Cancel::current()), f)
    }

    /// `call` that `cancel` stops at once (the request in flight is dropped, the connection stays).
    fn call_cancel<T, F, Fut>(&self, again: bool, cancel: Option<&Cancel>, f: F) -> Result<std::result::Result<T, SftpError>>
    where
        F: Fn(Arc<Conn>) -> Fut,
        Fut: Future<Output = std::result::Result<T, SftpError>>,
    {
        let wait = |fut: Fut| match cancel {
            Some(c) => vfs_rt::run_cancel(fut, c),
            None => Ok(run(fut)),
        };
        let c = self.conn()?;
        match wait(f(c.clone()))? {
            Err(e) if broken(&e) => {
                self.drop_conn(&c);
                if !again {
                    return Ok(Err(e));
                }
                let c = self.conn()?;
                let r = wait(f(c.clone()))?;
                if r.as_ref().is_err_and(broken) {
                    self.drop_conn(&c);
                }
                Ok(r)
            }
            r => Ok(r),
        }
    }

    fn lstat(&self, p: &VPath) -> Result<Option<FileAttributes>> {
        match self.call(true, |c| async move { c.raw.lstat(p.as_str()).await })? {
            Ok(a) => Ok(Some(a.attrs)),
            Err(e) if status_is(&e, StatusCode::NoSuchFile) => Ok(None),
            Err(e) => Err(err(p, e)),
        }
    }

    /// The server's own path with links resolved (`None`: it doesn't exist).
    fn realpath(&self, p: &VPath) -> Result<Option<VPath>> {
        match self.call(true, |c| async move { c.raw.realpath(p.as_str()).await })? {
            Ok(n) => Ok(n.files.into_iter().next().map(|f| VPath::new(&f.filename))),
            Err(e) if status_is(&e, StatusCode::NoSuchFile) => Ok(None),
            Err(e) => Err(err(p, e)),
        }
    }

    /// A symbolic link `link` → `target` (for tests: the contract has no links yet).
    pub fn make_symlink(&self, target: &VPath, link: &VPath) -> Result<()> {
        // OpenSSH swaps the arguments of SSH_FXP_SYMLINK against the draft: (target, link) on the wire.
        self.call(false, |c| async move { c.raw.symlink(target.as_str(), link.as_str()).await })?.map(|_| ()).map_err(|e| err(link, e))
    }

    fn set_attrs(&self, path: &VPath, f: impl Fn(&FileAttributes, &mut FileAttributes)) -> Result<()> {
        let now = self.call(true, |c| async move { c.raw.stat(path.as_str()).await })?.map_err(|e| err(path, e))?.attrs;
        let mut a = FileAttributes::empty();
        f(&now, &mut a);
        self.call(false, |c| {
            let a = a.clone();
            async move { c.raw.setstat(path.as_str(), a).await }
        })?
        .map(|_| ())
        .map_err(|e| err(path, e))
    }
}

impl Source for SftpFs {
    fn info(&self) -> SourceInfo {
        let port = if self.cfg.port == 22 { String::new() } else { format!(":{}", self.cfg.port) };
        SourceInfo {
            id: format!("sftp://{}@{}{port}", self.cfg.user, self.cfg.host),
            title: format!("sftp:{}@{}{port}", self.cfg.user, self.cfg.host),
            caps: Capabilities {
                write: true,
                rename: true,
                server_copy: self.copy_data,
                real_dirs: true,
                seek: true,
                permissions: true,
                set_mtime: true,
                case_sensitive: true,
            },
        }
    }

    fn roots(&self, _cancel: &Cancel) -> Result<Vec<Root>> {
        let mut roots = vec![Root { path: VPath::root(), title: "/".into() }, Root { path: self.home.clone(), title: "~".into() }];
        if !roots.iter().any(|r| r.path == self.start) {
            roots.push(Root { path: self.start.clone(), title: self.start.to_string() });
        }
        Ok(roots)
    }

    fn home(&self) -> VPath {
        self.start.clone()
    }

    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>> {
        cancel.check()?;
        let read = |c: Arc<Conn>| {
            let cancel = cancel.clone();
            async move {
                let h = c.raw.opendir(dir.as_str()).await?.handle;
                let mut files = Vec::new();
                let res = loop {
                    if cancel.is_cancelled() {
                        break Ok(());
                    }
                    match c.raw.readdir(h.as_str()).await {
                        Ok(n) => files.extend(n.files),
                        Err(e) if status_is(&e, StatusCode::Eof) => break Ok(()),
                        Err(e) => break Err(e),
                    }
                };
                let _ = c.raw.close(h).await;
                res.map(|_| (c, files))
            }
        };
        let (c, files) = match self.call_cancel(true, Some(cancel), read)? {
            Ok(x) => x,
            Err(e) => {
                // A file is "no such folder" to some servers: say what it is.
                if self.lstat(dir)?.is_some_and(|a| kind(&a) != Type::Dir && kind(&a) != Type::Link) {
                    return Err(Error::NotADirectory(dir.clone()));
                }
                return Err(err(dir, e));
            }
        };
        cancel.check()?;
        let files: Vec<_> = files.into_iter().filter(|f| f.filename != "." && f.filename != "..").collect();
        // Links: what each points to, all asked at once rather than two round trips per link in turn.
        let links: Vec<JoinHandle<_>> = files
            .iter()
            .filter(|f| kind(&f.attrs) == Type::Link)
            .map(|f| rt().spawn(follow(c.raw.clone(), dir.child(&f.filename).to_string())))
            .collect();
        let mut followed = run(async {
            let mut out = Vec::with_capacity(links.len());
            for h in links {
                out.push(h.await.unwrap_or((None, None)));
            }
            out
        })
        .into_iter();
        Ok(files.iter().map(|f| entry(&f.filename, &f.attrs, if kind(&f.attrs) == Type::Link { followed.next() } else { None })).collect())
    }

    fn stat(&self, path: &VPath) -> Result<Entry> {
        let a = self.lstat(path)?.ok_or_else(|| Error::NotFound(path.clone()))?;
        let link = if kind(&a) == Type::Link { Some(run(follow(self.conn()?.raw.clone(), path.to_string()))) } else { None };
        Ok(entry(path.name(), &a, link))
    }

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let a = self.call(true, |c| async move { c.raw.stat(path.as_str()).await })?.map_err(|e| err(path, e))?.attrs;
        if kind(&a) == Type::Dir {
            return Err(Error::IsADirectory(path.clone()));
        }
        if kind(&a) != Type::File {
            return Err(Error::msg(path, "not a regular file (a pipe, socket or device)"));
        }
        let open = |c: Arc<Conn>| async move { c.raw.open(path.as_str(), OpenFlags::READ, FileAttributes::empty()).await.map(|h| (c, h.handle)) };
        let (conn, handle) = self.call(true, open)?.map_err(|e| err(path, e))?;
        Ok(Box::new(Reader { conn, handle, path: path.clone(), next: offset, ahead: VecDeque::new(), depth: 1, buf: Vec::new(), pos: 0, eof: false }))
    }

    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn vfs::Writer>> {
        // Without replacing, what is there needn't be looked at now (a round trip per file): the rename at
        // `finish` never replaces, and says so.
        let there = if opts.overwrite { self.lstat(path)? } else { None };
        match &there {
            Some(a) if kind(a) == Type::Dir => return Err(Error::IsADirectory(path.clone())),
            Some(_) if !opts.overwrite => return Err(Error::AlreadyExists(path.clone())),
            _ => {}
        }
        // Over a link: write the file it points to, the link stays a link.
        let (target, old) = match there {
            Some(a) if kind(&a) == Type::Link => match self.realpath(path)? {
                Some(real) => {
                    let r = &real;
                    let old = self.call(true, |c| async move { c.raw.lstat(r.as_str()).await.map(|a| a.attrs) })?.ok();
                    (real, old)
                }
                None => (path.clone(), None),
            },
            a => (path.clone(), a),
        };
        if old.as_ref().is_some_and(|a| kind(a) == Type::Dir) {
            return Err(Error::IsADirectory(path.clone()));
        }
        let tmp = tmp_near(&target);
        // Replacing a file: the new one is born private and gets the old one's permissions before it takes
        // its place, so a 0600 file is never readable by others on the way. A new file: the server's umask.
        let mut attrs = FileAttributes::empty();
        if old.is_some() {
            attrs.permissions = Some(0o600);
        }
        let flags = OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE;
        let open = |c: Arc<Conn>| {
            let (tmp, attrs) = (tmp.clone(), attrs.clone());
            async move { c.raw.open(tmp.as_str(), flags, attrs).await.map(|h| (c, h.handle)) }
        };
        let (conn, handle) = self.call(false, open)?.map_err(|e| err(path, e))?;
        Ok(Box::new(Upload {
            conn,
            handle: Some(handle),
            tmp,
            path: target,
            shown: path.clone(),
            overwrite: opts.overwrite,
            modified: opts.modified,
            mode: opts.mode,
            old,
            at: 0,
            inflight: VecDeque::new(),
            done: false,
        }))
    }

    fn mkdir(&self, path: &VPath) -> Result<()> {
        if self.lstat(path)?.is_some() {
            return Err(Error::AlreadyExists(path.clone()));
        }
        self.call(false, |c| async move { c.raw.mkdir(path.as_str(), FileAttributes::empty()).await })?.map(|_| ()).map_err(|e| err(path, e))
    }

    fn remove(&self, path: &VPath) -> Result<()> {
        let a = self.lstat(path)?.ok_or_else(|| Error::NotFound(path.clone()))?;
        if kind(&a) != Type::Dir {
            return self.call(false, |c| async move { c.raw.remove(path.as_str()).await })?.map(|_| ()).map_err(|e| err(path, e));
        }
        self.call(false, |c| async move { c.raw.rmdir(path.as_str()).await })?.map(|_| ()).map_err(|e| {
            // Servers say only "failure" for a folder that is not empty.
            let full = self.list(path, &Cancel::new()).is_ok_and(|l| !l.is_empty());
            if full { Error::DirNotEmpty(path.clone()) } else { err(path, e) }
        })
    }

    /// All the REMOVE requests at once (64 in flight): a round trip for the lot, not one per file.
    fn remove_files(&self, paths: &[VPath], cancel: &Cancel) -> Vec<Result<()>> {
        let c = match self.conn() {
            Ok(c) => c,
            Err(e) => return paths.iter().map(|_| Err(e.clone())).collect(),
        };
        let mut out = Vec::with_capacity(paths.len());
        for chunk in paths.chunks(64) {
            let jobs: Vec<JoinHandle<_>> = chunk
                .iter()
                .map(|p| {
                    let (raw, p) = (c.raw.clone(), p.to_string());
                    rt().spawn(async move { raw.remove(p).await.map(|_| ()) })
                })
                .collect();
            let res = vfs_rt::run_cancel(
                async {
                    let mut v = Vec::with_capacity(jobs.len());
                    for j in jobs {
                        v.push(j.await);
                    }
                    v
                },
                cancel,
            );
            match res {
                Err(e) => out.extend(chunk.iter().map(|_| Err(e.clone()))),
                Ok(v) => out.extend(v.into_iter().zip(chunk).map(|(r, p)| match r {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(err(p, e)),
                    Err(e) => Err(Error::msg(p, e.to_string())),
                })),
            }
        }
        out
    }

    fn space(&self, dir: &VPath) -> Option<(u64, u64)> {
        let c = self.conn().ok()?;
        if !c.statvfs {
            return None;
        }
        let v = vfs_rt::run_current(c.raw.statvfs(dir.as_str())).ok()?.ok()?;
        let unit = if v.fragment_size > 0 { v.fragment_size } else { v.block_size };
        Some((v.blocks_avail.saturating_mul(unit), v.blocks.saturating_mul(unit)))
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        if to.starts_with(from) && to != from {
            return Err(Error::msg(to, "cannot move a folder into itself"));
        }
        // A change of case only: the "existing" target may be the source itself (a case-insensitive server).
        let case_only = from.as_str().to_lowercase() == to.as_str().to_lowercase();
        if !case_only && self.lstat(to)?.is_some() {
            return Err(Error::AlreadyExists(to.clone()));
        }
        // SSH_FXP_RENAME never replaces (OpenSSH links the new name, then unlinks the old).
        self.call(false, |c| async move { c.raw.rename(from.as_str(), to.as_str()).await })?.map(|_| ()).map_err(|e| err(from, e))
    }

    fn set_mtime(&self, path: &VPath, t: SystemTime) -> Result<()> {
        let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
        // SFTP sets both times at once: the access time stays.
        self.set_attrs(path, |now, a| {
            a.mtime = Some(secs);
            a.atime = now.atime.or(Some(secs));
        })
    }

    fn set_mode(&self, path: &VPath, mode: u32) -> Result<()> {
        self.set_attrs(path, |_, a| a.permissions = Some(mode & 0o7777))
    }

    /// The server copies the file itself (`copy-data`), in pieces so that cancelling stops it between them;
    /// the copy gets the source's time and permissions, and takes the name only when complete.
    fn copy_within(&self, from: &VPath, to: &VPath, cancel: &Cancel) -> Result<()> {
        if !self.copy_data {
            return Err(Error::Unsupported);
        }
        let src = self.call(true, |c| async move { c.raw.stat(from.as_str()).await })?.map_err(|e| err(from, e))?.attrs;
        if kind(&src) != Type::File {
            return Err(Error::Unsupported);
        }
        let size = src.size.unwrap_or(0);
        let c = self.conn()?;
        let raw = c.raw.clone();
        let rh = run(raw.open(from.as_str(), OpenFlags::READ, FileAttributes::empty())).map_err(|e| err(from, e))?.handle;
        let tmp = tmp_near(to);
        let wh = match run(raw.open(tmp.as_str(), OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE, FileAttributes::empty())) {
            Ok(h) => h.handle,
            Err(e) => {
                rt().spawn(async move { raw.close(rh).await });
                return Err(err(to, e));
            }
        };
        let cleanup = |raw: Arc<RawSftpSession>, rh: String, wh: String, tmp: VPath| {
            run(async move {
                let _ = tokio::join!(raw.close(rh), raw.close(wh));
                let _ = raw.remove(tmp.as_str()).await;
            })
        };
        // 256 MB per request: a few seconds of the server's disk, well inside the request timeout.
        const PIECE: u64 = 256 << 20;
        let mut at = 0;
        while at < size.max(1) {
            if cancel.is_cancelled() {
                cleanup(raw, rh, wh, tmp);
                return Err(Error::Cancelled);
            }
            let len = PIECE.min(size - at);
            let mut data = Vec::new();
            ssh_string(&mut data, &rh);
            data.extend_from_slice(&at.to_be_bytes());
            data.extend_from_slice(&len.to_be_bytes());
            ssh_string(&mut data, &wh);
            data.extend_from_slice(&at.to_be_bytes());
            let res = match run(raw.extended("copy-data", data)) {
                Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => Ok(()),
                Ok(Packet::Status(s)) => Err(err(to, SftpError::Status(s))),
                Ok(_) => Err(Error::msg(to, "unexpected answer to copy-data")),
                Err(e) => Err(err(to, e)),
            };
            if let Err(e) = res {
                cleanup(raw, rh, wh, tmp);
                return Err(e);
            }
            if size == 0 {
                break;
            }
            at += len;
        }
        let mut a = FileAttributes::empty();
        a.permissions = src.permissions.map(|m| m & 0o1777);
        (a.mtime, a.atime) = (src.mtime, src.atime.or(src.mtime));
        let (r1, r2, r3, w1, w2) = (raw.clone(), raw.clone(), raw.clone(), wh.clone(), wh);
        let ((), _, closed) = run(async move {
            tokio::join!(
                async move {
                    let _ = r1.fsetstat(w1, a).await;
                },
                r2.close(rh),
                r3.close(w2)
            )
        });
        if let Err(e) = closed {
            run(raw.remove(tmp.as_str())).ok();
            return Err(err(to, e));
        }
        // Into place, replacing what is there (the user agreed already).
        let place = if c.posix_rename {
            let mut data = Vec::new();
            ssh_string(&mut data, tmp.as_str());
            ssh_string(&mut data, to.as_str());
            match run(raw.extended("posix-rename@openssh.com", data)) {
                Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => Ok(()),
                Ok(Packet::Status(s)) => Err(err(to, SftpError::Status(s))),
                Ok(_) => Err(Error::msg(to, "unexpected answer to posix-rename")),
                Err(e) => Err(err(to, e)),
            }
        } else {
            let _ = run(raw.remove(to.as_str()));
            run(raw.rename(tmp.as_str(), to.as_str())).map(|_| ()).map_err(|e| err(to, e))
        };
        if place.is_err() {
            let _ = run(raw.remove(tmp.as_str()));
        }
        place
    }

    /// One object when the server resolves both paths (links included) to the same one.
    fn same_file(&self, a: &VPath, b: &VPath) -> bool {
        a == b || matches!((self.realpath(a), self.realpath(b)), (Ok(Some(x)), Ok(Some(y))) if x == y)
    }
}

/// One read request in flight: the bytes, or `None` at the end of the file.
type ReadJob = JoinHandle<std::result::Result<Option<Vec<u8>>, SftpError>>;

/// A file being read: up to `depth` requests ahead of the reader, each for the next `chunk` bytes.
struct Reader {
    conn: Arc<Conn>,
    handle: String,
    path: VPath,
    /// Where the next request starts.
    next: u64,
    /// Requests in flight, in file order: (offset, length, answer; `None` — the end of the file).
    ahead: VecDeque<(u64, u32, ReadJob)>,
    /// Grows ×2 per answer up to `depth()`: a short read (the viewer) asks little, a copy soon asks a lot.
    depth: usize,
    buf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl Reader {
    fn ask(&mut self) {
        while !self.eof && self.ahead.len() < self.depth {
            let (raw, h, at, len) = (self.conn.raw.clone(), self.handle.clone(), self.next, self.conn.chunk);
            let job = rt().spawn(async move {
                match raw.read(h, at, len).await {
                    Ok(d) => Ok(Some(d.data)),
                    Err(e) if status_is(&e, StatusCode::Eof) => Ok(None),
                    Err(e) => Err(e),
                }
            });
            self.ahead.push_back((at, len, job));
            self.next += len as u64;
        }
    }
}

impl Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.pos >= self.buf.len() {
            if self.eof {
                return Ok(0);
            }
            self.ask();
            let Some((at, len, job)) = self.ahead.pop_front() else { return Ok(0) };
            let got = vfs_rt::run_current(job)
                .map_err(|e| io::Error::other(e.to_string()))?
                .map_err(io::Error::other)?
                .map_err(|e| io::Error::other(err(&self.path, e).to_string()))?;
            self.depth = (self.depth * 2).min(depth());
            match got {
                None => {
                    self.eof = true;
                    self.ahead.clear();
                }
                Some(d) => {
                    // A short answer: the requests behind it start at the wrong place — ask again from here.
                    if (d.len() as u32) < len {
                        self.ahead.clear();
                        self.next = at + d.len() as u64;
                        if d.is_empty() {
                            self.eof = true;
                        }
                    }
                    self.buf = d;
                    self.pos = 0;
                }
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let (raw, h) = (self.conn.raw.clone(), std::mem::take(&mut self.handle));
        rt().spawn(async move {
            let _ = raw.close(h).await;
        });
    }
}

/// A file being written under a temporary name, up to `depth()` writes in flight; `finish` gives it the
/// real name.
struct Upload {
    conn: Arc<Conn>,
    handle: Option<String>,
    tmp: VPath,
    /// Where it goes (a link's target when writing over a link).
    path: VPath,
    /// What the caller asked for — in errors.
    shown: VPath,
    overwrite: bool,
    /// The time and permissions asked for at create.
    modified: Option<SystemTime>,
    mode: Option<u32>,
    /// The file being replaced: its permissions and owner go to the new one (unless `mode` says otherwise).
    old: Option<FileAttributes>,
    at: u64,
    inflight: VecDeque<JoinHandle<std::result::Result<(), SftpError>>>,
    done: bool,
}

impl Upload {
    fn wait_one(&mut self) -> io::Result<()> {
        if let Some(job) = self.inflight.pop_front() {
            vfs_rt::run_current(job)
                .map_err(|e| io::Error::other(e.to_string()))?
                .map_err(io::Error::other)?
                .map_err(|e| io::Error::other(err(&self.shown, e).to_string()))?;
        }
        Ok(())
    }
}

impl Write for Upload {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let h = self.handle.clone().ok_or_else(|| io::Error::other("closed"))?;
        for piece in data.chunks(self.conn.chunk as usize) {
            while self.inflight.len() >= depth() {
                self.wait_one()?;
            }
            let (raw, h, at, piece) = (self.conn.raw.clone(), h.clone(), self.at, piece.to_vec());
            self.at += piece.len() as u64;
            self.inflight.push_back(rt().spawn(async move { raw.write(h, at, piece).await.map(|_| ()) }));
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        while !self.inflight.is_empty() {
            self.wait_one()?;
        }
        Ok(())
    }
}

impl vfs::Writer for Upload {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let p = self.shown.clone();
        self.flush().map_err(|e| Error::msg(&p, e.to_string()))?;
        let raw = self.conn.raw.clone();
        // Time and permissions on the open file, then close — sent together, one round trip. The owner of a
        // replaced file can be set only by root: then it is asked separately, and its failure doesn't matter.
        let mut a = FileAttributes::empty();
        a.permissions = self.mode.or(self.old.as_ref().and_then(|o| o.permissions)).map(|m| m & 0o7777);
        if let Some(t) = self.modified {
            let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as u32).unwrap_or(0);
            (a.mtime, a.atime) = (Some(secs), Some(secs));
        }
        let owner = self.old.as_ref().map(|o| (o.uid, o.gid));
        if let Some(h) = self.handle.take() {
            let set = a.permissions.is_some() || a.mtime.is_some();
            // `join!` polls in order and each request goes out on its first poll: owner, attributes, close
            // reach the server in that order without waiting for each other.
            let (r1, r2, r3) = (raw.clone(), raw.clone(), raw.clone());
            let (h1, h2) = (h.clone(), h.clone());
            let owner = async move {
                if let Some((uid, gid)) = owner {
                    let mut o = FileAttributes::empty();
                    (o.uid, o.gid) = (uid, gid);
                    let _ = r1.fsetstat(h1, o).await;
                }
            };
            let attrs = async move {
                if set {
                    let _ = r2.fsetstat(h2, a).await;
                }
            };
            // A server that can't set the owner, time or permissions still has the data right.
            let ((), (), closed) = run(async move { tokio::join!(owner, attrs, r3.close(h)) });
            closed.map_err(|e| err(&p, e))?;
        }
        let (tmp, path) = (self.tmp.to_string(), self.path.to_string());
        if self.overwrite && self.conn.posix_rename {
            let mut data = Vec::new();
            ssh_string(&mut data, &tmp);
            ssh_string(&mut data, &path);
            return match run(raw.extended("posix-rename@openssh.com", data)) {
                Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => {
                    self.done = true;
                    Ok(())
                }
                Ok(Packet::Status(s)) => Err(err(&p, SftpError::Status(s))),
                Ok(_) => Err(Error::msg(&p, "unexpected answer to posix-rename")),
                Err(e) => Err(err(&p, e)),
            };
        }
        // Plain SFTP rename never replaces. Over an existing file: the old one steps aside first and comes back
        // if the new one can't take its place.
        let aside = tmp_near(&self.path).to_string();
        let replaced = self.overwrite && run(raw.rename(path.clone(), aside.clone())).is_ok();
        match run(raw.rename(tmp, path.clone())) {
            Ok(_) => {
                if replaced {
                    let _ = run(raw.remove(aside));
                }
                self.done = true;
                Ok(())
            }
            Err(e) => {
                if replaced {
                    let _ = run(raw.rename(aside, path.clone()));
                }
                let exists = run(raw.lstat(path)).is_ok();
                Err(if exists && !self.overwrite { Error::AlreadyExists(p) } else { err(&p, e) })
            }
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if !self.done {
            let raw = self.conn.raw.clone();
            let (h, tmp) = (self.handle.take(), self.tmp.to_string());
            let inflight = std::mem::take(&mut self.inflight);
            run(async move {
                for j in inflight {
                    let _ = j.await;
                }
                if let Some(h) = h {
                    let _ = raw.close(h).await;
                }
                let _ = raw.remove(tmp).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_strings() {
        assert_eq!(parse_server("me@host").unwrap(), ("me".into(), "host".into(), 22));
        assert_eq!(parse_server("me@host:2222").unwrap(), ("me".into(), "host".into(), 2222));
        assert_eq!(parse_server(" me@[::1]:2200 ").unwrap(), ("me".into(), "::1".into(), 2200));
        assert_eq!(parse_server("a@b@host").unwrap().0, "a@b");
        assert!(parse_server("me@host:x").is_err());
        assert!(parse_server("me@").is_err());
        assert_eq!(split_server("box").unwrap(), (None, "box".into(), None));
    }

    #[test]
    fn the_start_folder() {
        let home = VPath::new("/home/me");
        for (typed, at) in
            [("", "/home/me"), (" ~ ", "/home/me"), ("www", "/home/me/www"), ("~/www/", "/home/me/www"), ("/srv/data/me", "/srv/data/me")]
        {
            assert_eq!(start(&home, typed), VPath::new(at), "{typed:?}");
        }
    }

    #[test]
    fn ssh_config_as_ssh_reads_it() {
        let text = "
            # global
            User everyone
            Host work
                HostName work.example.com
                Port 2200
                IdentityFile ~/.ssh/id_work
            Host *.lan !printer.lan
                User pi
            Host *
                User fallback
                IdentityFile ~/.ssh/id_default
        ";
        let w = ssh_config(text, "work");
        assert_eq!(w.host_name.as_deref(), Some("work.example.com"));
        assert_eq!((w.port, w.identity_file.as_deref()), (Some(2200), Some("~/.ssh/id_work")));
        assert_eq!(w.user.as_deref(), Some("everyone"), "the first value wins, also from before any Host");
        let lan = ssh_config("Host *.lan !printer.lan\n  User pi\nHost *\n  User other\n", "nas.lan");
        assert_eq!(lan.user.as_deref(), Some("pi"));
        let printer = ssh_config("Host *.lan !printer.lan\n  User pi\nHost *\n  User other\n", "printer.lan");
        assert_eq!(printer.user.as_deref(), Some("other"));
        assert_eq!(ssh_config(text, "unknown").host_name, None);
        assert_eq!(ssh_config_hosts(text), ["work"]);
        assert_eq!(ssh_config("Host=a b\n", "b").host_name, None);
        assert_eq!(ssh_config_hosts("Host=a b\n"), ["a", "b"]);
    }
}
