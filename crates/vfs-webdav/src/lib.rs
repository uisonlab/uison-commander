//! Files on a WebDAV server (Nextcloud, ownCloud, Apache mod_dav, nginx, rclone) as a vfs source.
//!
//! A folder is PROPFIND (Depth 1), a file GET with a Range (the viewer reads from any offset). A file is
//! written next to its place under a temporary name (PUT, streamed as it is written) and moved over it at
//! `finish` (MOVE): an unfinished write leaves nothing and spoils nothing. MKCOL makes a folder, DELETE removes
//! (a folder only when empty: WebDAV deletes a whole tree), MOVE renames (`Overwrite: F` — never replaces),
//! COPY copies on the server. Basic authentication, http and https (the system's certificates).
//!
//! The contract is blocking and called from workers; the requests run on the shared runtime of `vfs-rt` and
//! stop on the worker's `Cancel`.

use std::io::{self, Read, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body::Frame;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use vfs::{Cancel, Capabilities, CreateOpts, Entry, Error, Kind, Result, Root, Source, SourceInfo, VPath};

/// No connection in this long: the server is not there.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No answer to a request in this long (its headers; a listing's body too).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// A file being read or written that moves no data for this long: the connection is dead.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes a writer gathers before handing them to the upload.
const CHUNK: usize = 256 << 10;

#[derive(Clone, Debug, Default)]
pub struct Config {
    /// `https://host/remote.php/dav/files/me/`: the folder the source starts in (its `/`).
    pub url: String,
    pub user: String,
    pub password: String,
}

type Body = BoxBody<Bytes, io::Error>;
type HttpClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

/// What a worker and a writer share: the client and where the server is.
struct Inner {
    client: HttpClient,
    /// `https://host:port`.
    origin: String,
    /// The base folder's path as sent (percent-encoded), ending with `/`.
    base: String,
    /// The same, decoded: hrefs in answers are compared with it.
    base_decoded: String,
    /// `Basic …`, or none.
    auth: Option<String>,
    /// `host:port`, for messages.
    host: String,
}

pub struct DavFs {
    inner: Arc<Inner>,
    cfg: Config,
}

static TEMP: AtomicU64 = AtomicU64::new(0);

impl DavFs {
    /// Check the address and sign in (a PROPFIND of the base folder): a wrong password or a server that is
    /// not WebDAV fails here, not at the first listing.
    pub fn connect(cfg: Config) -> Result<DavFs> {
        let (origin, base) = split_url(&cfg.url).map_err(|m| Error::Io { path: None, msg: m })?;
        let host = origin.split("://").nth(1).unwrap_or(&origin).to_string();
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_native_roots(Arc::new(rustls::crypto::ring::default_provider()))
            .map_err(|e| Error::Io { path: None, msg: format!("the system's certificates: {e}") })?
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new()).build(https);
        let auth = (!cfg.user.is_empty()).then(|| format!("Basic {}", base64(format!("{}:{}", cfg.user, cfg.password).as_bytes())));
        let base_decoded = decode(&base);
        let fs = DavFs { inner: Arc::new(Inner { client, origin, base, base_decoded, auth, host }), cfg };
        let root = VPath::root();
        let e = fs.inner.propfind(&root, true, 0, &Cancel::current())?;
        match e.first() {
            Some((_, e)) if e.is_dir() => Ok(fs),
            _ => Err(Error::Io { path: None, msg: format!("{}: not a WebDAV folder", fs.cfg.url) }),
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    fn exists(&self, path: &VPath) -> Result<bool> {
        match self.stat(path) {
            Ok(_) => Ok(true),
            Err(Error::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

impl Inner {
    /// The URL of `path` (a folder's ends with `/`).
    fn url(&self, path: &VPath, dir: bool) -> String {
        let mut u = format!("{}{}", self.origin, self.base);
        let parts: Vec<String> = path.components().map(encode).collect();
        u.push_str(&parts.join("/"));
        if dir && !parts.is_empty() {
            u.push('/');
        }
        u
    }

    /// A request, its answer's headers within `ANSWER_TIMEOUT`, stopped by `cancel`.
    fn send(&self, req: Request<Body>, path: &VPath, cancel: &Cancel) -> Result<Response<Incoming>> {
        cancel.check()?;
        let fut = timed(ANSWER_TIMEOUT, self.client.request(req));
        match vfs_rt::run_cancel(fut, cancel)? {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(Error::Io { path: Some(path.clone()), msg: format!("{}: {}", self.host, chain(&e)) }),
            Err(_) => Err(Error::Io { path: Some(path.clone()), msg: format!("{}: no answer in {} s", self.host, ANSWER_TIMEOUT.as_secs()) }),
        }
    }

    fn request(&self, method: &str, url: &str) -> hyper::http::request::Builder {
        let mut b = Request::builder().method(Method::from_bytes(method.as_bytes()).expect("method")).uri(url);
        if let Some(a) = &self.auth {
            b = b.header("Authorization", a);
        }
        b
    }

    /// A request without a body to speak of, and its whole answer.
    fn call(&self, req: Request<Body>, path: &VPath, cancel: &Cancel) -> Result<(StatusCode, Bytes)> {
        let r = self.send(req, path, cancel)?;
        let status = r.status();
        let fut = timed(ANSWER_TIMEOUT, r.into_body().collect());
        match vfs_rt::run_cancel(fut, cancel)? {
            Ok(Ok(b)) => Ok((status, b.to_bytes())),
            Ok(Err(e)) => Err(Error::Io { path: Some(path.clone()), msg: format!("{}: {}", self.host, chain(&e)) }),
            Err(_) => Err(Error::Io { path: Some(path.clone()), msg: format!("{}: no answer in {} s", self.host, ANSWER_TIMEOUT.as_secs()) }),
        }
    }

    fn simple(&self, method: &str, path: &VPath, dir: bool, headers: &[(&str, String)]) -> Result<StatusCode> {
        let mut b = self.request(method, &self.url(path, dir));
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        let req = b.body(empty()).map_err(|e| Error::msg(path, e.to_string()))?;
        Ok(self.call(req, path, &Cancel::current())?.0)
    }

    /// The entries of `path` (Depth 1: itself and its children) as (path relative to the base, entry).
    fn propfind(&self, path: &VPath, dir: bool, depth: u8, cancel: &Cancel) -> Result<Vec<(String, Entry)>> {
        const BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/><D:getlastmodified/></D:prop></D:propfind>"#;
        let req = self
            .request("PROPFIND", &self.url(path, dir))
            .header("Depth", depth.to_string())
            .header("Content-Type", "application/xml; charset=utf-8")
            .body(full(BODY.as_bytes()))
            .map_err(|e| Error::msg(path, e.to_string()))?;
        let (status, body) = self.call(req, path, cancel)?;
        // A folder asked for without its slash: some servers send it to the address with one.
        if !dir && status.is_redirection() {
            return self.propfind(path, true, depth, cancel);
        }
        if status != StatusCode::MULTI_STATUS {
            return Err(status_error(path, status, "PROPFIND"));
        }
        let text = String::from_utf8_lossy(&body);
        Ok(parse_multistatus(&text).into_iter().map(|r| (self.relative(&r.href), r.entry())).collect())
    }

    /// An href of an answer (a path or a whole URL, percent-encoded) as a path under the base, without
    /// slashes at the ends.
    fn relative(&self, href: &str) -> String {
        let path = match href.split_once("://") {
            Some((_, rest)) => rest.find('/').map_or("/", |i| &rest[i..]),
            None => href,
        };
        let d = decode(path);
        let rel = d.strip_prefix(&self.base_decoded).or_else(|| d.strip_prefix(self.base_decoded.trim_end_matches('/'))).unwrap_or(&d);
        rel.trim_matches('/').to_string()
    }

    fn delete(&self, path: &VPath, dir: bool) -> Result<()> {
        match self.simple("DELETE", path, dir, &[])? {
            s if s.is_success() => Ok(()),
            s => Err(status_error(path, s, "DELETE")),
        }
    }

    /// MOVE or COPY of `from` to `to`, replacing it or not.
    fn transfer(&self, method: &str, from: &VPath, to: &VPath, dir: bool, overwrite: bool) -> Result<()> {
        let h = [("Destination", self.url(to, dir)), ("Overwrite", if overwrite { "T" } else { "F" }.to_string())];
        match self.simple(method, from, dir, &h)? {
            s if s.is_success() => Ok(()),
            StatusCode::PRECONDITION_FAILED => Err(Error::AlreadyExists(to.clone())),
            // 409: a folder on the way to the target is missing.
            StatusCode::CONFLICT => Err(Error::NotFound(to.parent().unwrap_or_else(VPath::root))),
            s => Err(status_error(from, s, method)),
        }
    }
}

impl Source for DavFs {
    fn info(&self) -> SourceInfo {
        let base = self.inner.base_decoded.trim_end_matches('/');
        let user = if self.cfg.user.is_empty() { String::new() } else { format!("{}@", self.cfg.user) };
        SourceInfo {
            id: format!("webdav://{user}{}{base}", self.inner.host),
            title: format!("webdav:{}{base}", self.inner.host),
            caps: Capabilities {
                write: true,
                rename: true,
                server_copy: true,
                real_dirs: true,
                seek: true,
                permissions: false,
                // getlastmodified is the server's to set (PROPPATCH of it is refused).
                set_mtime: false,
                case_sensitive: true,
            },
        }
    }

    fn roots(&self, _cancel: &Cancel) -> Result<Vec<Root>> {
        Ok(vec![Root { path: VPath::root(), title: "/".into() }])
    }

    fn home(&self) -> VPath {
        VPath::root()
    }

    fn list(&self, dir: &VPath, cancel: &Cancel) -> Result<Vec<Entry>> {
        let me = dir.as_str().trim_matches('/').to_string();
        let all = match self.inner.propfind(dir, true, 1, cancel) {
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            // A file asked for as a folder (with a slash): Apache answers 400.
            Err(e) => return Err(if self.stat(dir).is_ok_and(|e| !e.is_dir()) { Error::NotADirectory(dir.clone()) } else { e }),
            Ok(all) => all,
        };
        // A file answers with itself only.
        if let [(rel, e)] = all.as_slice()
            && *rel == me
            && !e.is_dir()
        {
            return Err(Error::NotADirectory(dir.clone()));
        }
        Ok(all.into_iter().filter(|(rel, _)| *rel != me).map(|(_, e)| e).collect())
    }

    fn stat(&self, path: &VPath) -> Result<Entry> {
        let all = self.inner.propfind(path, false, 0, &Cancel::current())?;
        let (_, mut e) = all.into_iter().next().ok_or_else(|| Error::NotFound(path.clone()))?;
        e.name = path.name().to_string();
        e.hidden = e.name.starts_with('.');
        Ok(e)
    }

    fn open_read(&self, path: &VPath, offset: u64) -> Result<Box<dyn Read + Send>> {
        let mut b = self.inner.request("GET", &self.inner.url(path, false));
        if offset > 0 {
            b = b.header("Range", format!("bytes={offset}-"));
        }
        let req = b.body(empty()).map_err(|e| Error::msg(path, e.to_string()))?;
        let r = self.inner.send(req, path, &Cancel::current())?;
        let skip = match r.status() {
            StatusCode::PARTIAL_CONTENT => 0,
            // The whole file: the server doesn't do ranges; what is before the offset is passed over.
            StatusCode::OK => offset,
            // At or past the end.
            StatusCode::RANGE_NOT_SATISFIABLE => return Ok(Box::new(io::empty())),
            s => return Err(status_error(path, s, "GET")),
        };
        let mut reader = BodyReader { body: r.into_body(), buf: Bytes::new(), path: path.clone() };
        if skip > 0 {
            io::copy(&mut (&mut reader).take(skip), &mut io::sink()).map_err(|e| Error::io(path, &e))?;
        }
        Ok(Box::new(reader))
    }

    fn create(&self, path: &VPath, opts: &CreateOpts) -> Result<Box<dyn vfs::Writer>> {
        if !opts.overwrite && self.exists(path)? {
            return Err(Error::AlreadyExists(path.clone()));
        }
        let parent = path.parent().unwrap_or_else(VPath::root);
        let temp = parent.child(&format!(".uc-{}-{}.tmp", std::process::id(), TEMP.fetch_add(1, Ordering::Relaxed)));
        let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let mut b = self.inner.request("PUT", &self.inner.url(&temp, false));
        if let Some(n) = opts.size_hint {
            b = b.header("Content-Length", n.to_string());
        }
        let req = b.body(ChanBody { rx, left: opts.size_hint }.boxed()).map_err(|e| Error::msg(path, e.to_string()))?;
        let inner = self.inner.clone();
        // The upload runs while the writer is written to; its answer comes at `finish`.
        let upload = vfs_rt::rt().spawn(async move { inner.client.request(req).await });
        Ok(Box::new(DavWriter {
            inner: self.inner.clone(),
            path: path.clone(),
            temp,
            overwrite: opts.overwrite,
            buf: Vec::with_capacity(CHUNK),
            tx: Some(tx),
            upload: Some(upload),
        }))
    }

    fn mkdir(&self, path: &VPath) -> Result<()> {
        // Some servers (rclone) answer 201 to MKCOL of a folder that is there.
        if self.exists(path)? {
            return Err(Error::AlreadyExists(path.clone()));
        }
        match self.inner.simple("MKCOL", path, true, &[])? {
            s if s.is_success() => Ok(()),
            StatusCode::METHOD_NOT_ALLOWED => Err(Error::AlreadyExists(path.clone())),
            // 409: the parent is missing.
            StatusCode::CONFLICT => Err(Error::NotFound(path.parent().unwrap_or_else(VPath::root))),
            s => Err(status_error(path, s, "MKCOL")),
        }
    }

    fn remove(&self, path: &VPath) -> Result<()> {
        let e = self.stat(path)?;
        if e.is_dir() {
            // DELETE of a folder takes all in it: the contract removes only an empty one.
            if !self.list(path, &Cancel::current())?.is_empty() {
                return Err(Error::DirNotEmpty(path.clone()));
            }
            return self.inner.delete(path, true);
        }
        self.inner.delete(path, false)
    }

    fn rename(&self, from: &VPath, to: &VPath) -> Result<()> {
        let dir = self.stat(from)?.is_dir();
        // `Overwrite: F` is not enough: Apache answers a MOVE of a file onto a folder with a redirect. A change
        // of case only is not "there already" (a server that ignores case finds the source by the new name).
        let case_only = from.as_str().to_lowercase() == to.as_str().to_lowercase();
        if !case_only && self.exists(to)? {
            return Err(Error::AlreadyExists(to.clone()));
        }
        self.inner.transfer("MOVE", from, to, dir, false)
    }

    fn copy_within(&self, from: &VPath, to: &VPath, _cancel: &Cancel) -> Result<()> {
        if self.stat(from)?.is_dir() {
            return Err(Error::Unsupported);
        }
        self.inner.transfer("COPY", from, to, false, true)
    }
}

/// A file being read: the body of a GET, a frame at a time.
struct BodyReader {
    body: Incoming,
    buf: Bytes,
    path: VPath,
}

impl Read for BodyReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.buf.is_empty() {
            let next = timed(STALL_TIMEOUT, self.body.frame());
            match vfs_rt::run_current(next) {
                Err(e) => return Err(io::Error::other(e.to_string())),
                Ok(Err(_)) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, format!("{}: no data for {} s", self.path, STALL_TIMEOUT.as_secs())));
                }
                Ok(Ok(None)) => return Ok(0),
                Ok(Ok(Some(Err(e)))) => return Err(io::Error::other(format!("{}: {}", self.path, chain(&e)))),
                Ok(Ok(Some(Ok(f)))) => {
                    if let Ok(d) = f.into_data() {
                        self.buf = d;
                    }
                }
            }
        }
        let n = out.len().min(self.buf.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf = self.buf.slice(n..);
        Ok(n)
    }
}

/// The body of an upload: what the writer sends through the channel; `left` — the length given in advance.
struct ChanBody {
    rx: tokio::sync::mpsc::Receiver<Bytes>,
    left: Option<u64>,
}

impl http_body::Body for ChanBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<std::result::Result<Frame<Bytes>, io::Error>>> {
        self.rx.poll_recv(cx).map(|b| {
            b.map(|b| {
                if let Some(l) = &mut self.left {
                    *l = l.saturating_sub(b.len() as u64);
                }
                Ok(Frame::data(b))
            })
        })
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self.left {
            Some(n) => http_body::SizeHint::with_exact(n),
            None => http_body::SizeHint::default(),
        }
    }
}

type Upload = tokio::task::JoinHandle<std::result::Result<Response<Incoming>, hyper_util::client::legacy::Error>>;

/// A file being written under a temporary name; `finish` moves it into its place.
struct DavWriter {
    inner: Arc<Inner>,
    path: VPath,
    temp: VPath,
    overwrite: bool,
    buf: Vec<u8>,
    tx: Option<tokio::sync::mpsc::Sender<Bytes>>,
    upload: Option<Upload>,
}

impl DavWriter {
    fn flush_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(CHUNK)));
        let tx = self.tx.as_ref().ok_or_else(|| io::Error::other("finished"))?;
        // The upload took no more (it failed): `finish` says why.
        tx.blocking_send(chunk).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, format!("{}: the upload stopped", self.path)))
    }

    /// The upload's answer, after its body is complete.
    fn wait(&mut self) -> Result<StatusCode> {
        self.tx = None;
        let Some(up) = self.upload.take() else { return Err(Error::msg(&self.path, "the upload was finished")) };
        match vfs_rt::run_current(up)? {
            Ok(Ok(r)) => {
                let status = r.status();
                let _ = vfs_rt::run_current(r.into_body().collect());
                Ok(status)
            }
            Ok(Err(e)) => Err(Error::Io { path: Some(self.path.clone()), msg: format!("{}: {}", self.inner.host, chain(&e)) }),
            Err(e) => Err(Error::msg(&self.path, e.to_string())),
        }
    }

    fn clean_up(&self) {
        let _ = self.inner.delete(&self.temp, false);
    }
}

impl Write for DavWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= CHUNK {
            self.flush_buf()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl vfs::Writer for DavWriter {
    fn finish(mut self: Box<Self>) -> Result<()> {
        let sent = self.flush_buf();
        let status = self.wait();
        let status = match (sent, status) {
            (_, Err(e)) => Err(e),
            (Err(e), Ok(_)) => Err(Error::io(&self.path, &e)),
            (Ok(()), Ok(s)) => Ok(s),
        };
        let done = match status {
            Ok(s) if s.is_success() => self.inner.transfer("MOVE", &self.temp, &self.path, false, self.overwrite),
            // 409: the folder is missing.
            Ok(StatusCode::CONFLICT) => Err(Error::NotFound(self.path.parent().unwrap_or_else(VPath::root))),
            Ok(s) => Err(status_error(&self.path, s, "PUT")),
            Err(e) => Err(e),
        };
        if done.is_err() {
            self.clean_up();
        }
        done
    }
}

impl Drop for DavWriter {
    /// Dropped without `finish`: the upload is stopped mid-body (never ended, so never complete) and its
    /// temporary file goes.
    fn drop(&mut self) {
        if let Some(up) = self.upload.take() {
            up.abort();
            let _ = vfs_rt::run(up);
            self.tx = None;
            self.clean_up();
        }
    }
}

/// `tokio::time::timeout` made inside the runtime (its timer needs one), when the future is first polled.
async fn timed<F: std::future::Future>(d: Duration, f: F) -> std::result::Result<F::Output, tokio::time::error::Elapsed> {
    tokio::time::timeout(d, f).await
}

fn empty() -> Body {
    Full::new(Bytes::new()).map_err(|n| match n {}).boxed()
}

fn full(b: &'static [u8]) -> Body {
    Full::new(Bytes::from_static(b)).map_err(|n| match n {}).boxed()
}

/// An error and its causes: "client error (Connect): tcp connect error: Connection refused".
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        cur = c.source();
    }
    s
}

/// An answer that is not what was asked for, as an error of `path`.
fn status_error(path: &VPath, s: StatusCode, what: &str) -> Error {
    let p = Some(path.clone());
    match s {
        StatusCode::NOT_FOUND => Error::NotFound(path.clone()),
        StatusCode::FORBIDDEN => Error::PermissionDenied(path.clone()),
        StatusCode::UNAUTHORIZED => Error::Io { path: p, msg: "the server did not accept the user name or password".into() },
        StatusCode::LOCKED => Error::Io { path: p, msg: "locked by another program".into() },
        StatusCode::INSUFFICIENT_STORAGE => Error::Io { path: p, msg: "no space left on the server".into() },
        _ => Error::Io { path: p, msg: format!("{what}: HTTP {} {}", s.as_u16(), s.canonical_reason().unwrap_or("")).trim_end().to_string() },
    }
}

/// `https://host:8443/dav/files/me` → (`https://host:8443`, `/dav/files/me/`), the path percent-encoded.
pub fn split_url(url: &str) -> std::result::Result<(String, String), String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("\"{url}\" is not an address: type http://… or https://…"))?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(format!("\"{url}\": only http:// and https:// addresses"));
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("\"{url}\": no server in the address"));
    }
    // What was typed with spaces or letters of any language goes out encoded; `%20` typed stays as it is.
    let mut base: String = path.split('/').map(|seg| encode(&decode(seg))).collect::<Vec<_>>().join("/");
    if !base.ends_with('/') {
        base.push('/');
    }
    Ok((format!("{}://{authority}", scheme.to_ascii_lowercase()), base))
}

/// A path segment for a URL: everything but letters, digits and `-._~` as `%XX`.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `%XX` back to bytes, read as UTF-8.
pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            out.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// One `<response>` of a PROPFIND answer.
#[derive(Debug, Default, PartialEq)]
struct DavResponse {
    href: String,
    collection: bool,
    length: Option<u64>,
    modified: Option<SystemTime>,
}

impl DavResponse {
    fn entry(&self) -> Entry {
        let name = decode(self.href.trim_end_matches('/').rsplit('/').next().unwrap_or(""));
        let mut e = Entry::new(name, if self.collection { Kind::Dir } else { Kind::File });
        e.size = if self.collection { None } else { self.length };
        e.modified = self.modified;
        e.hidden = e.name.starts_with('.');
        e
    }
}

/// The responses of a `multistatus` answer, whatever the namespace prefixes (`D:`, `d:`, `lp1:` or none).
fn parse_multistatus(xml: &str) -> Vec<DavResponse> {
    use xmlparser::{ElementEnd, Token};
    let mut out = Vec::new();
    let mut cur: Option<DavResponse> = None;
    // Open elements by local name; the text goes to the innermost.
    let mut stack: Vec<String> = Vec::new();
    for t in xmlparser::Tokenizer::from(xml) {
        let Ok(t) = t else { break };
        match t {
            Token::ElementStart { local, .. } => {
                let name = local.as_str().to_string();
                match name.as_str() {
                    "response" => cur = Some(DavResponse::default()),
                    "collection" if stack.iter().any(|s| s == "resourcetype") => {
                        if let Some(r) = &mut cur {
                            r.collection = true;
                        }
                    }
                    _ => {}
                }
                stack.push(name);
            }
            Token::ElementEnd { end, .. } => {
                let closed = match end {
                    ElementEnd::Open => continue,
                    ElementEnd::Empty | ElementEnd::Close(..) => stack.pop(),
                };
                if closed.as_deref() == Some("response")
                    && let Some(r) = cur.take()
                {
                    out.push(r);
                }
            }
            Token::Text { text } | Token::Cdata { text, .. } => {
                let (Some(r), Some(top)) = (&mut cur, stack.last()) else { continue };
                let v = unescape(text.as_str().trim());
                match top.as_str() {
                    "href" => r.href.push_str(&v),
                    "getcontentlength" => r.length = v.parse().ok(),
                    "getlastmodified" => r.modified = http_date(&v),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

/// `&amp;` and the rest, `&#233;`, `&#xE9;`.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find(';') else {
            out.push('&');
            rest = after;
            continue;
        };
        let ent = &after[..j];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")).map_or_else(
                || ent.strip_prefix('#').and_then(|n| n.parse().ok()).and_then(char::from_u32),
                |h| u32::from_str_radix(h, 16).ok().and_then(char::from_u32),
            ),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &after[j + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `Sun, 06 Nov 1994 08:49:37 GMT` (RFC 1123, what WebDAV servers send).
fn http_date(s: &str) -> Option<SystemTime> {
    let p: Vec<&str> = s.split_whitespace().collect();
    let [_, day, mon, year, time, ..] = p.as_slice() else { return None };
    let mon = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].iter().position(|m| m == mon)? as i64 + 1;
    let (d, y): (i64, i64) = (day.parse().ok()?, year.parse().ok()?);
    let t: Vec<i64> = time.split(':').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [h, mi, se] = t.as_slice() else { return None };
    // Days from 1970-01-01 (H. Hinnant's days_from_civil).
    let y2 = if mon <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mon > 2 { mon - 3 } else { mon + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + se;
    u64::try_from(secs).ok().map(|s| UNIX_EPOCH + Duration::from_secs(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_names() {
        assert_eq!(split_url("https://h:8443/dav/files/me").unwrap(), ("https://h:8443".into(), "/dav/files/me/".into()));
        assert_eq!(split_url("http://h").unwrap(), ("http://h".into(), "/".into()));
        assert_eq!(split_url("http://h/my docs/%D0%96").unwrap().1, "/my%20docs/%D0%96/");
        assert!(split_url("ftp://h/").is_err() && split_url("h/dav").is_err());
        assert_eq!(encode("проценты %20 и #хеш.md"), "%D0%BF%D1%80%D0%BE%D1%86%D0%B5%D0%BD%D1%82%D1%8B%20%2520%20%D0%B8%20%23%D1%85%D0%B5%D1%88.md");
        assert_eq!(decode(&encode("emoji 😀 %")), "emoji 😀 %");
        assert_eq!(base64(b"uison:test-password"), "dWlzb246dGVzdC1wYXNzd29yZA==");
        assert_eq!((base64(b"a"), base64(b"ab")), ("YQ==".to_string(), "YWI=".to_string()));
    }

    #[test]
    fn dates() {
        assert_eq!(http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(UNIX_EPOCH + Duration::from_secs(784_111_777)));
        assert_eq!(http_date("Sun, 08 Nov 2026 00:29:10 GMT"), Some(UNIX_EPOCH + Duration::from_secs(1_794_097_750)));
        assert_eq!(http_date("yesterday"), None);
    }

    /// Apache's answer (other prefixes, a 404 propstat, an entity), and rclone's.
    #[test]
    fn multistatus() {
        let apache = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:">
<D:response xmlns:lp1="DAV:"><D:href>/dav/files/me/</D:href>
<D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype>
<lp1:getlastmodified>Sun, 06 Nov 1994 08:49:37 GMT</lp1:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
<D:propstat><D:prop><D:getcontentlength/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat></D:response>
<D:response><D:href>/dav/files/me/A%20%26%20B.txt</D:href><D:propstat><D:prop><lp1:resourcetype/>
<lp1:getcontentlength>12</lp1:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>
<D:response><D:href>http://h/dav/files/me/R&amp;D/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response>
</D:multistatus>"#;
        let r = parse_multistatus(apache);
        assert_eq!(r.len(), 3);
        assert!(r[0].collection && r[0].modified.is_some() && r[0].length.is_none());
        assert_eq!((r[1].entry().name.as_str(), r[1].length, r[1].collection), ("A & B.txt", Some(12), false));
        assert_eq!((r[2].entry().name.as_str(), r[2].collection), ("R&D", true));
        let rclone = r#"<?xml version="1.0" encoding="UTF-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href><D:propstat><D:prop><D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype><D:getlastmodified>Sun, 08 Nov 2026 00:29:10 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let r = parse_multistatus(rclone);
        assert!(r.len() == 1 && r[0].collection && r[0].href == "/");
    }
}
