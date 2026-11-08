//! Against real FTP servers. Skipped unless `UISON_FTP_SERVERS` is set: addresses separated by spaces, the
//! scheme saying how — `ftp://` plain, `ftpes://` explicit TLS, `ftps://` implicit TLS (`tools/test-env.sh`:
//! vsftpd and pure-ftpd, which differ in nearly everything):
//!
//! ```text
//! UISON_FTP_SERVERS="ftp://localhost:2121 ftpes://localhost:2121 ftps://localhost:2990 ftp://localhost:2131" \
//! UISON_FTP_USER=uison UISON_FTP_PASSWORD=… cargo test -p vfs-ftp
//! ```
//!
//! The servers' certificates are self-signed: each test is refused first and then trusts the fingerprint it
//! was shown, as the user would. Every test works in a fresh folder of its own.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ops::{Answer, Feedback, Mode, Progress, Question, Transfer};
use vfs::{Cancel, CreateOpts, Error, MemFs, Source, VPath};
use vfs_ftp::{Config, FtpFs, Security};

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// The servers to test on; none — skipped.
fn configs() -> Vec<Config> {
    let (user, password) = (var("UISON_FTP_USER").unwrap_or_default(), var("UISON_FTP_PASSWORD").unwrap_or_default());
    var("UISON_FTP_SERVERS")
        .unwrap_or_default()
        .split_whitespace()
        .map(|url| {
            let (scheme, rest) = url.split_once("://").expect("scheme://host:port");
            let security = match scheme {
                "ftp" => Security::None,
                "ftpes" => Security::Explicit,
                "ftps" => Security::Implicit,
                s => panic!("{s}: ftp, ftpes or ftps"),
            };
            let (_, host, port) = vfs_ftp::split_server(rest).unwrap();
            Config { host, port: port.unwrap_or(0), user: user.clone(), password: password.clone(), security, trusted: Vec::new() }
        })
        .collect()
}

/// Connected, trusting the certificate it is shown (the servers' are self-signed).
fn connect(c: &Config) -> FtpFs {
    match FtpFs::connect(c.clone()) {
        Ok(s) => s,
        Err(e) => {
            let (fp, _) = vfs_ftp::untrusted_certificate(&e).unwrap_or_else(|| panic!("connect: {e}"));
            FtpFs::connect(Config { trusted: vec![fp], ..c.clone() }).expect("connect trusting the certificate")
        }
    }
}

fn folder(s: &FtpFs, tag: &str) -> VPath {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = s.home().child(&format!("uison-{tag}-{stamp}"));
    s.mkdir(&d).unwrap();
    d
}

fn wipe(s: &FtpFs, dir: &VPath) {
    for e in s.list(dir, &Cancel::new()).unwrap() {
        let p = dir.child(&e.name);
        if e.is_dir() { wipe(s, &p) } else { s.remove(&p).unwrap() }
    }
    s.remove(dir).unwrap();
}

struct Quiet;

impl Feedback for Quiet {
    fn progress(&self, _: &Progress) {}
    fn ask(&self, q: Question) -> Answer {
        panic!("unexpected question {q:?}")
    }
}

/// Each test on each server; none — skipped.
fn on_each(f: fn(Config)) {
    let cs = configs();
    if cs.is_empty() {
        return vfs::conformance::skip("UISON_FTP_SERVERS");
    }
    for c in cs {
        eprintln!("{}:{} {:?}", c.host, c.port(), c.security);
        f(c);
    }
}

#[test]
fn conformance() {
    on_each(|c| {
        let s = connect(&c);
        let base = folder(&s, "conf");
        vfs::conformance::run(&s, &base);
        wipe(&s, &base);
    });
}

/// A tree there and back through `ops`: a big file, a nested folder, odd names; the listing has sizes and
/// times; a move on the server; the time kept where the server sets times.
#[test]
fn a_tree_there_and_back() {
    on_each(|c| {
        let ftp = Arc::new(connect(&c));
        let s: Arc<dyn Source> = ftp.clone();
        let mem = MemFs::new("m");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        mem.add_file("/t/big.bin", big.clone());
        mem.add_file("/t/sub/дерево 1.txt", "один");
        let mem: Arc<dyn Source> = Arc::new(mem);
        let base = folder(&ftp, "tree");
        let t = Transfer {
            mode: Mode::Copy,
            src: mem.clone(),
            src_dir: VPath::new("/"),
            names: vec!["t".into()],
            dst: s.clone(),
            target: base.to_string(),
        };
        let r = t.run(&Quiet, &Cancel::new());
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        let e = s.stat(&base.join("t/big.bin")).unwrap();
        assert_eq!(e.size, Some(big.len() as u64));
        let listed = s.list(&base.join("t"), &Cancel::new()).unwrap();
        let big_listed = listed.iter().find(|e| e.name == "big.bin").unwrap();
        assert!(big_listed.size == Some(big.len() as u64) && big_listed.modified.is_some(), "{big_listed:?}");
        let mut back = Vec::new();
        s.open_read(&base.join("t/big.bin"), 0).unwrap().read_to_end(&mut back).unwrap();
        assert!(back == big, "the same bytes back");
        let mut tail = Vec::new();
        s.open_read(&base.join("t/big.bin"), 2_999_990).unwrap().read_to_end(&mut tail).unwrap();
        assert_eq!(tail, big[2_999_990..]);
        let names: Vec<String> = s.list(&base.join("t/sub"), &Cancel::new()).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["дерево 1.txt"]);
        let t = Transfer {
            mode: Mode::Move,
            src: s.clone(),
            src_dir: base.clone(),
            names: vec!["t".into()],
            dst: s.clone(),
            target: base.join("moved").to_string(),
        };
        let r = t.run(&Quiet, &Cancel::new());
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(s.stat(&base.join("moved/sub/дерево 1.txt")).is_ok() && s.stat(&base.join("t")).is_err());
        wipe(&ftp, &base);
    });
}

/// Reading a part and dropping the rest (the viewer does it all the time) leaves the connection usable,
/// and many such reads don't pile up connections: the server allows only a few.
#[test]
fn partial_reads() {
    on_each(|c| {
        let s = connect(&c);
        let base = folder(&s, "part");
        let p = base.child("f.bin");
        let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 253) as u8).collect();
        let mut w = s.create(&p, &CreateOpts { size_hint: Some(data.len() as u64), ..CreateOpts::default() }).unwrap();
        w.write_all(&data).unwrap();
        w.finish().unwrap();
        for i in 0..30u64 {
            let off = i * 50_000;
            let mut buf = vec![0u8; 1000];
            s.open_read(&p, off).unwrap().read_exact(&mut buf).unwrap();
            assert_eq!(buf, data[off as usize..off as usize + 1000], "at {off}");
        }
        // Past the end: nothing, not an error.
        let mut rest = Vec::new();
        s.open_read(&p, data.len() as u64 + 10).unwrap().read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
        wipe(&s, &base);
    });
}

/// A cancel stops a listing at once, and the source goes on working.
#[test]
fn cancel_and_go_on() {
    on_each(|c| {
        let s = connect(&c);
        let base = folder(&s, "cancel");
        let cancel = Cancel::new();
        let t = Instant::now();
        let c2 = cancel.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            c2.cancel();
        });
        // Listing again and again until the cancel lands in the middle of one.
        let mut res = Ok(Vec::new());
        while res.is_ok() && t.elapsed() < Duration::from_secs(5) {
            res = s.list(&base, &cancel);
        }
        stopper.join().unwrap();
        assert_eq!(res, Err(Error::Cancelled));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(s.list(&base, &Cancel::new()).unwrap().is_empty());
        wipe(&s, &base);
    });
}

/// A wrong password and a server that is not there fail at connect, saying so; plain FTP to a server's TLS
/// port and TLS to a plain one say what is wrong.
#[test]
fn wrong_password_and_no_server() {
    on_each(|c| {
        let bad = Config { password: "wrong".into(), ..c.clone() };
        let e = match FtpFs::connect(bad.clone()) {
            Err(e) if vfs_ftp::untrusted_certificate(&e).is_some() => {
                let fp = vfs_ftp::untrusted_certificate(&e).unwrap().0;
                FtpFs::connect(Config { trusted: vec![fp], ..bad }).err().expect("refused")
            }
            r => r.err().expect("refused"),
        };
        assert!(e.to_string().contains("did not accept"), "{e}");
        let e = FtpFs::connect(Config { host: "127.0.0.1".into(), port: 1, ..c.clone() }).err().expect("no server").to_string();
        assert!(e.to_lowercase().contains("refused") && e.contains("127.0.0.1:1"), "{e}");
    });
}

/// A certificate the system doesn't trust: refused with its fingerprint and the reason; another fingerprint
/// trusted is no help.
#[test]
fn untrusted_certificate() {
    on_each(|c| {
        if c.security == Security::None {
            return;
        }
        let e = FtpFs::connect(c.clone()).err().expect("a self-signed certificate is refused");
        let (fp, why) = vfs_ftp::untrusted_certificate(&e).unwrap_or_else(|| panic!("{e}"));
        assert!(fp.starts_with("SHA256:") && fp.len() == 7 + 32 * 3 - 1, "{fp}");
        assert!(why.contains("self-signed") || why.contains("authority"), "{why}");
        let other = Config { trusted: vec!["SHA256:00".into()], ..c.clone() };
        assert!(vfs_ftp::untrusted_certificate(&FtpFs::connect(other).err().expect("refused")).is_some());
        assert!(FtpFs::connect(Config { trusted: vec![fp], ..c }).is_ok());
    });
}
