//! One control connection to an FTP server: commands and their replies, data connections, TLS.
//!
//! Every socket is read and written in short waits (`TICK`): between them the worker's `Cancel` and the time
//! limit are checked, so a server that stops answering or a cancel stops a call within a fraction of a second.
//! A connection whose exchange was cut (cancel, timeout, a broken socket) is `broken`: its state is unknown, so it
//! is never used again.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{Resumption, WebPkiServerVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme, StreamOwned};
use vfs::Cancel;

/// No connection in this long: the server is not there.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// No reply to a command in this long.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);
/// A transfer that moves no data for this long: the connection is dead.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a socket waits before the cancel and the limit are looked at again.
const TICK: Duration = Duration::from_millis(200);

/// Why an exchange stopped. Either way the connection is not used again.
#[derive(Debug)]
pub enum Fail {
    Cancelled,
    /// The connection is gone or the server is silent: text for a person, without the server's name.
    Lost(String),
    /// TLS refused the server's certificate: its SHA-256 fingerprint and why.
    Untrusted {
        fingerprint: String,
        why: String,
    },
}

/// A reply: its code and its text (the lines after the code, joined by `\n`).
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub code: u16,
    pub text: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.code)
    }
}

impl std::fmt::Display for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.code, self.text.replace('\n', " "))
    }
}

/// TLS to one server: the certificate check and the server's name.
///
/// The data connections must resume the TLS session of their control connection (vsftpd refuses others with
/// `522`, and a vsftpd session lives in the process of its control connection). So each control connection has a
/// session cache of its own (`session`), and TLS 1.2 comes first: rustls uses a TLS 1.3 ticket only once, and an
/// upload reads nothing from its data connection, so it gets no new one — after two uploads the tickets are
/// gone. A TLS 1.2 session resumes any number of times. A server without TLS 1.2 gets 1.3.
pub struct Tls {
    tls12: ClientConfig,
    any: ClientConfig,
    /// The server refused TLS 1.2.
    tls13: AtomicBool,
    name: ServerName<'static>,
    verifier: Arc<Verifier>,
}

/// One control connection's TLS: its configuration with its own session cache.
#[derive(Clone)]
pub struct Session {
    config: Arc<ClientConfig>,
    tls: Arc<Tls>,
}

impl Tls {
    /// Certificates the system trusts, and those with a fingerprint in `trusted` (the user said yes to them).
    pub fn new(host: &str, trusted: Vec<String>) -> Result<Tls, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let webpki =
            WebPkiServerVerifier::builder_with_provider(roots(), provider.clone()).build().map_err(|e| format!("the system's certificates: {e}"))?;
        let verifier = Arc::new(Verifier { webpki, trusted, provider: provider.clone(), refused: Mutex::new(None) });
        let config = |versions: &[&'static rustls::SupportedProtocolVersion]| -> Result<ClientConfig, String> {
            Ok(ClientConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(versions)
                .map_err(|e| e.to_string())?
                .dangerous()
                .with_custom_certificate_verifier(verifier.clone())
                .with_no_client_auth())
        };
        let name = ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: {e}"))?;
        Ok(Tls { tls12: config(&[&rustls::version::TLS12])?, any: config(rustls::DEFAULT_VERSIONS)?, tls13: AtomicBool::new(false), name, verifier })
    }

    /// The TLS of a new control connection.
    pub fn session(self: &Arc<Self>) -> Session {
        let mut c = if self.tls13.load(Ordering::Relaxed) { self.any.clone() } else { self.tls12.clone() };
        c.resumption = Resumption::in_memory_sessions(4);
        Session { config: Arc::new(c), tls: self.clone() }
    }
}

impl Session {
    /// A TLS stream over `tcp`, its handshake done.
    fn wrap(&self, tcp: TcpStream) -> Result<Stream, Fail> {
        let tls = &self.tls;
        let conn = ClientConnection::new(self.config.clone(), tls.name.clone()).map_err(|e| Fail::Lost(format!("TLS: {e}")))?;
        let mut s = StreamOwned::new(conn, tcp);
        let deadline = Instant::now() + ANSWER_TIMEOUT;
        while s.conn.is_handshaking() {
            match s.conn.complete_io(&mut s.sock) {
                Ok(_) => {}
                Err(e) if waiting(&e) => tick(deadline, "no answer to the TLS handshake")?,
                Err(e) => {
                    if let Some((fingerprint, why)) = tls.verifier.refused.lock().unwrap().take() {
                        return Err(Fail::Untrusted { fingerprint, why });
                    }
                    // A server without TLS 1.2: 1.3 from the next connection on.
                    if format!("{e:?}").contains("ProtocolVersion") && !tls.tls13.swap(true, Ordering::Relaxed) {
                        return Err(Fail::Lost(RETRY_TLS13.into()));
                    }
                    return Err(Fail::Lost(format!("TLS: {e}")));
                }
            }
        }
        Ok(Stream::Tls(Box::new(s)))
    }
}

/// The first TLS handshake learnt that the server has no TLS 1.2: connect again.
pub const RETRY_TLS13: &str = "TLS 1.2 refused";

/// The system's root certificates, read once (the Keychain takes a while).
fn roots() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut store = RootCertStore::empty();
            store.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            Arc::new(store)
        })
        .clone()
}

/// `SHA256:` and the digest of the certificate in hex pairs, as browsers show it.
pub fn fingerprint(cert: &[u8]) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, cert);
    let hex: Vec<String> = d.as_ref().iter().map(|b| format!("{b:02X}")).collect();
    format!("SHA256:{}", hex.join(":"))
}

/// The system's verdict, unless the user trusts this very certificate.
#[derive(Debug)]
struct Verifier {
    webpki: Arc<WebPkiServerVerifier>,
    trusted: Vec<String>,
    provider: Arc<CryptoProvider>,
    /// The last certificate refused: its fingerprint and why.
    refused: Mutex<Option<(String, String)>>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity.as_ref());
        if self.trusted.contains(&fp) {
            return Ok(ServerCertVerified::assertion());
        }
        self.webpki.verify_server_cert(end_entity, intermediates, server_name, ocsp, now).inspect_err(|e| {
            *self.refused.lock().unwrap() = Some((fp, cert_error(e)));
        })
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Why the system refused a certificate, in words.
fn cert_error(e: &rustls::Error) -> String {
    use rustls::CertificateError as C;
    match e {
        rustls::Error::InvalidCertificate(c) => match c {
            C::UnknownIssuer => "it is not signed by an authority this Mac trusts (a self-signed certificate?)".into(),
            C::NotValidForName | C::NotValidForNameContext { .. } => "it is issued for another name".into(),
            C::Expired | C::ExpiredContext { .. } => "it has expired".into(),
            C::NotValidYet | C::NotValidYetContext { .. } => "it is not valid yet".into(),
            C::Revoked => "it has been revoked".into(),
            // A certificate that signs itself (webpki: a CA where a server's is expected).
            other if format!("{other:?}").contains("CaUsedAsEndEntity") => "it is self-signed".into(),
            other => format!("{other:?}"),
        },
        other => other.to_string(),
    }
}

/// A read or write that timed out (`TICK`): look at the cancel and the limit and try again.
fn waiting(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted)
}

/// Between two waits: stopped by the worker's cancel, or by `deadline` (`what` says what didn't come).
fn tick(deadline: Instant, what: &str) -> Result<(), Fail> {
    if Cancel::current().is_cancelled() {
        return Err(Fail::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Fail::Lost(what.to_string()));
    }
    Ok(())
}

enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
    /// Handed over to TLS (for a moment, at `AUTH TLS`).
    Gone,
}

impl Stream {
    fn tcp(&self) -> Option<&TcpStream> {
        match self {
            Stream::Plain(s) => Some(s),
            Stream::Tls(s) => Some(&s.sock),
            Stream::Gone => None,
        }
    }

    /// Reads what comes within `limit`; 0 at the end. A TLS peer that closes without saying so is the end
    /// too: the server's reply on the control connection tells whether the transfer was complete.
    fn read_some(&mut self, buf: &mut [u8], limit: Duration, what: &str) -> Result<usize, Fail> {
        let deadline = Instant::now() + limit;
        loop {
            let r = match self {
                Stream::Plain(s) => s.read(buf),
                Stream::Tls(s) => s.read(buf),
                Stream::Gone => Err(io::ErrorKind::NotConnected.into()),
            };
            match r {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) if waiting(&e) => tick(deadline, what)?,
                Err(e) => return Err(Fail::Lost(e.to_string())),
            }
        }
    }

    fn write_all_in(&mut self, mut data: &[u8], limit: Duration) -> Result<(), Fail> {
        let deadline = Instant::now() + limit;
        while !data.is_empty() {
            let r = match self {
                Stream::Plain(s) => s.write(data),
                Stream::Tls(s) => s.write(data),
                Stream::Gone => Err(io::ErrorKind::NotConnected.into()),
            };
            match r {
                Ok(0) => return Err(Fail::Lost("the connection is closed".into())),
                Ok(n) => data = &data[n..],
                Err(e) if waiting(&e) => tick(deadline, "the server takes no data")?,
                Err(e) => return Err(Fail::Lost(e.to_string())),
            }
        }
        // What rustls holds back goes out now.
        if let Stream::Tls(s) = self {
            loop {
                match s.flush() {
                    Ok(()) => break,
                    Err(e) if waiting(&e) => tick(deadline, "the server takes no data")?,
                    Err(e) => return Err(Fail::Lost(e.to_string())),
                }
            }
        }
        Ok(())
    }

    /// The end of a data connection: TLS says so (servers want it after an upload), then the socket closes.
    fn close(mut self) {
        if let Stream::Tls(s) = &mut self {
            s.conn.send_close_notify();
            let deadline = Instant::now() + Duration::from_secs(5);
            while s.conn.wants_write() {
                match s.conn.write_tls(&mut s.sock) {
                    Ok(_) => {}
                    Err(e) if waiting(&e) && Instant::now() < deadline => {}
                    Err(_) => break,
                }
            }
        }
        if let Some(s) = self.tcp() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

fn plain_socket(addr: SocketAddr) -> Result<TcpStream, Fail> {
    let s = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(|e| Fail::Lost(e.to_string()))?;
    let _ = s.set_nodelay(true);
    s.set_read_timeout(Some(TICK)).and_then(|()| s.set_write_timeout(Some(TICK))).map_err(|e| Fail::Lost(e.to_string()))?;
    Ok(s)
}

/// A data connection: a file's bytes or a listing.
pub struct Data {
    stream: Stream,
}

impl Data {
    pub fn close(self) {
        self.stream.close();
    }
}

impl Read for Data {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read_some(buf, STALL_TIMEOUT, &format!("no data for {} s", STALL_TIMEOUT.as_secs())).map_err(io_error)
    }
}

impl Write for Data {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write_all_in(buf, STALL_TIMEOUT).map_err(io_error)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A failure as an I/O error (`Read`/`Write` say no more).
pub fn io_error(f: Fail) -> io::Error {
    match f {
        Fail::Cancelled => io::Error::other("cancelled"),
        Fail::Lost(m) => io::Error::new(io::ErrorKind::TimedOut, m),
        Fail::Untrusted { why, .. } => io::Error::other(why),
    }
}

/// How the control connection is made safe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Security {
    /// `AUTH TLS` on the ordinary port (21): FTPS as most servers do it.
    #[default]
    Explicit,
    /// TLS from the first byte (port 990).
    Implicit,
    /// None: the password and the files go in the clear.
    None,
}

pub struct Conn {
    stream: Stream,
    /// What came from the server and is not a reply yet.
    buf: Vec<u8>,
    /// The server's address: passive data connections go there.
    peer: SocketAddr,
    /// Data connections in TLS (`PROT P`), resuming this connection's session.
    tls: Option<Session>,
    /// `EPSV` was refused: `PASV` from now on.
    no_epsv: bool,
    /// The exchange was cut: the connection is in an unknown state.
    pub broken: bool,
}

impl Conn {
    /// Connects, reads the greeting and, for FTPS, secures the control and data connections.
    pub fn open(addr: SocketAddr, security: Security, tls: Option<&Arc<Tls>>) -> Result<Conn, Fail> {
        match Conn::open_once(addr, security, tls) {
            Err(Fail::Lost(m)) if m == RETRY_TLS13 => Conn::open_once(addr, security, tls),
            r => r,
        }
    }

    fn open_once(addr: SocketAddr, security: Security, tls: Option<&Arc<Tls>>) -> Result<Conn, Fail> {
        let tls = tls.filter(|_| security != Security::None).map(Tls::session);
        let tcp = plain_socket(addr)?;
        let stream = match (security, &tls) {
            (Security::Implicit, Some(t)) => t.wrap(tcp)?,
            _ => Stream::Plain(tcp),
        };
        let mut c = Conn { stream, buf: Vec::new(), peer: addr, tls: None, no_epsv: false, broken: false };
        let hello = c.reply()?;
        if hello.code != 220 {
            return Err(Fail::Lost(format!("the server refused: {hello}")));
        }
        if let (Security::Explicit, Some(t)) = (security, &tls) {
            let r = c.cmd("AUTH TLS")?;
            if r.code != 234 {
                return Err(Fail::Lost(format!("the server does not offer TLS ({r}): choose FTP without encryption to connect anyway")));
            }
            if let Stream::Plain(tcp) = std::mem::replace(&mut c.stream, Stream::Gone) {
                c.stream = t.wrap(tcp)?;
            }
        }
        if tls.is_some() {
            c.expect("PBSZ 0", 200)?;
            c.expect("PROT P", 200)?;
            c.tls = tls;
        }
        Ok(c)
    }

    /// A command and its reply. Lines that would end the command early are refused here.
    pub fn cmd(&mut self, line: &str) -> Result<Reply, Fail> {
        if line.contains(['\r', '\n']) {
            return Err(Fail::Lost("a line break in a name can't be sent over FTP".into()));
        }
        self.send(line)?;
        self.reply()
    }

    /// A command that must answer `code`; any other is the connection's failure (used where no other answer
    /// is expected: setting it up).
    pub fn expect(&mut self, line: &str, code: u16) -> Result<Reply, Fail> {
        let r = self.cmd(line)?;
        if r.code != code {
            return Err(Fail::Lost(format!("{}: {r}", line.split(' ').next().unwrap_or(line))));
        }
        Ok(r)
    }

    fn send(&mut self, line: &str) -> Result<(), Fail> {
        let mut out = Vec::with_capacity(line.len() + 2);
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
        self.stream.write_all_in(&out, ANSWER_TIMEOUT).inspect_err(|_| self.broken = true)
    }

    /// The next reply (one line, or `123-` … `123 ` lines).
    pub fn reply(&mut self) -> Result<Reply, Fail> {
        self.reply_in(ANSWER_TIMEOUT)
    }

    fn reply_in(&mut self, limit: Duration) -> Result<Reply, Fail> {
        let r = self.read_reply(limit);
        if r.is_err() {
            self.broken = true;
        }
        r
    }

    fn read_reply(&mut self, limit: Duration) -> Result<Reply, Fail> {
        let first = self.line(limit)?;
        let code: u16 = first.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| Fail::Lost(format!("not an FTP reply: {first:?}")))?;
        let mut text = first.get(4..).unwrap_or("").to_string();
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let l = self.line(limit)?;
                if l.starts_with(&end) || l == end.trim_end() {
                    let last = l.get(4..).unwrap_or("").trim();
                    if !last.is_empty() {
                        text.push('\n');
                        text.push_str(last);
                    }
                    break;
                }
                text.push('\n');
                text.push_str(l.strip_prefix(&format!("{code}-")).unwrap_or(&l));
            }
        }
        let r = Reply { code, text: text.trim_end().to_string() };
        // 421: the server is closing the connection (idle too long, shutting down).
        if r.code == 421 {
            return Err(Fail::Lost(format!("the server closed the connection: {}", r.text)));
        }
        Ok(r)
    }

    /// One line of the control connection, without its line end.
    fn line(&mut self, limit: Duration) -> Result<String, Fail> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=i).collect();
                let s = String::from_utf8_lossy(&raw);
                return Ok(s.trim_end_matches(['\r', '\n']).to_string());
            }
            let n = self.stream.read_some(&mut chunk, limit, &format!("no answer in {} s", limit.as_secs()))?;
            if n == 0 {
                return Err(Fail::Lost("the server closed the connection".into()));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Where the next data connection goes: `EPSV`, or `PASV` (only its port: the address is the control
    /// connection's — behind NAT a server names its inner one). `Err(reply)` — the server refused both.
    fn passive(&mut self) -> Result<Result<SocketAddr, Reply>, Fail> {
        if !self.no_epsv {
            let r = self.cmd("EPSV")?;
            if r.code == 229 {
                // `229 Entering Extended Passive Mode (|||6446|)`
                let port = r.text.split('|').filter_map(|p| p.parse::<u16>().ok()).next_back();
                return match port {
                    Some(p) => Ok(Ok(SocketAddr::new(self.peer.ip(), p))),
                    None => Err(Fail::Lost(format!("EPSV: {r}"))),
                };
            }
            if r.code < 500 {
                return Ok(Err(r));
            }
            self.no_epsv = true;
        }
        let r = self.cmd("PASV")?;
        if r.code != 227 {
            return Ok(Err(r));
        }
        // `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`
        let nums: Vec<u16> = r.text.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        match nums.len() {
            n if n >= 6 => Ok(Ok(SocketAddr::new(self.peer.ip(), nums[n - 2] * 256 + nums[n - 1]))),
            _ => Err(Fail::Lost(format!("PASV: {r}"))),
        }
    }

    /// A data command (`RETR`, `STOR`, `LIST`…) from byte `rest`: its data connection, or the server's refusal.
    pub fn transfer(&mut self, line: &str, rest: u64) -> Result<Result<Data, Reply>, Fail> {
        let addr = match self.passive()? {
            Ok(a) => a,
            Err(r) => return Ok(Err(r)),
        };
        let tcp = plain_socket(addr).inspect_err(|_| self.broken = true)?;
        if rest > 0 {
            let r = self.cmd(&format!("REST {rest}"))?;
            if r.code != 350 {
                return Ok(Err(r));
            }
        }
        let r = self.cmd(line)?;
        if r.code != 150 && r.code != 125 {
            return Ok(Err(r));
        }
        let stream = match &self.tls {
            Some(t) => t.wrap(tcp).inspect_err(|_| self.broken = true)?,
            None => Stream::Plain(tcp),
        };
        Ok(Ok(Data { stream }))
    }

    /// The reply that ends a transfer (226), after its data connection is closed.
    pub fn done(&mut self) -> Result<Reply, Fail> {
        self.reply_in(STALL_TIMEOUT)
    }

    /// A listing's lines (`MLSD`, `LIST`): read whole, as UTF-8 (what isn't is replaced).
    pub fn lines(&mut self, line: &str) -> Result<Result<Vec<String>, Reply>, Fail> {
        let mut data = match self.transfer(line, 0)? {
            Ok(d) => d,
            Err(r) => return Ok(Err(r)),
        };
        let mut all = Vec::new();
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            let n = data.stream.read_some(&mut chunk, STALL_TIMEOUT, "the listing stopped coming").inspect_err(|_| self.broken = true)?;
            if n == 0 {
                break;
            }
            all.extend_from_slice(&chunk[..n]);
        }
        data.close();
        let r = self.done()?;
        if !r.ok() {
            return Ok(Err(r));
        }
        let text = String::from_utf8_lossy(&all);
        Ok(Ok(text.lines().map(|l| l.trim_end_matches('\r').to_string()).filter(|l| !l.is_empty()).collect()))
    }

    /// Stops a transfer midway (`ABOR`) and makes sure no reply of it is left unread. Servers answer it
    /// differently — `426` and `226` (pure-ftpd reading), `426` and `451` (pure-ftpd writing), `226` of the
    /// transfer and `225` (vsftpd) — so: one reply, one more after a `426`, then a `NOOP` and everything up to its `200`.
    /// The `NOOP` waits for the first replies: pure-ftpd drops what comes right after `ABOR`. Whatever goes
    /// wrong — the connection is not used again.
    pub fn abort(&mut self, data: Data) {
        let ok = (|| -> Result<(), Fail> {
            self.send("ABOR")?;
            data.close();
            let limit = Duration::from_secs(10);
            let deadline = Instant::now() + limit;
            if self.reply_in(limit)?.code == 426 {
                self.reply_in(limit)?;
            }
            self.send("NOOP")?;
            while self.reply_in(limit)?.code != 200 {
                tick(deadline, "ABOR")?;
            }
            Ok(())
        })();
        if ok.is_err() {
            self.broken = true;
        }
    }
}
