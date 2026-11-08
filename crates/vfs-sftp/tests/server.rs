//! Against a real SSH server. Skipped unless `UISON_SFTP_SERVER` is set, e.g. a test sshd in Docker:
//!
//! ```text
//! UISON_SFTP_SERVER=uison@localhost:2222 UISON_SFTP_PASSWORD=… \
//! UISON_SFTP_KEY=path/id_plain UISON_SFTP_KEY_PASS=path/id_pass UISON_SFTP_PASSPHRASE=… cargo test -p vfs-sftp
//! ```
//!
//! Every test keeps its known hosts in a file of its own and works in a fresh folder in the home.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ops::{Answer, Feedback, Mode, Progress, Question, Transfer};
use vfs::{Cancel, MemFs, Source, VPath};
use vfs_sftp::{Config, SftpFs, Trust, parse_server};

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

fn stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
}

/// A config for the test server, with a known hosts file of its own; `None` — no server.
fn config(tag: &str) -> Option<Config> {
    let (user, host, port) = parse_server(&var("UISON_SFTP_SERVER")?).unwrap();
    let known_hosts = std::env::temp_dir().join(format!("uison-sftp-{tag}-{}", stamp()));
    Some(Config { host, port, user, known_hosts, trust: Trust::Any, ..Config::default() })
}

fn with_password(c: Config) -> Config {
    Config { password: var("UISON_SFTP_PASSWORD"), ..c }
}

fn folder(s: &SftpFs, tag: &str) -> VPath {
    let d = s.home().child(&format!("uison-{tag}-{}", stamp()));
    s.mkdir(&d).unwrap();
    d
}

fn wipe(s: &SftpFs, dir: &VPath) {
    for e in s.list(dir, &Cancel::new()).unwrap() {
        let p = dir.child(&e.name);
        if e.kind == vfs::Kind::Dir { wipe(s, &p) } else { s.remove(&p).unwrap() }
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

#[test]
fn passes_the_contract() {
    let Some(c) = config("conf") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let base = folder(&s, "conf");
    vfs::conformance::run(&s, &base);
    s.remove(&base).unwrap();
}

#[test]
fn signs_in_every_way_and_checks_the_host() {
    let Some(c) = config("auth") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    // A new host is refused until its fingerprint is trusted — and only that fingerprint.
    let ask = Config { trust: Trust::Ask, ..with_password(c.clone()) };
    let e = SftpFs::connect(ask.clone()).err().expect("an unknown host is asked about");
    let fp = vfs_sftp::unknown_host(&e).unwrap_or_else(|| panic!("{e}"));
    assert!(fp.starts_with("SHA256:"), "{fp}");
    let wrong = Config { trust: Trust::Fingerprint("SHA256:not-this-one".into()), ..ask.clone() };
    assert!(SftpFs::connect(wrong).is_err(), "another fingerprint is not trusted");
    let (_, note) = SftpFs::connect(Config { trust: Trust::Fingerprint(fp), ..ask.clone() }).unwrap();
    assert!(note.as_deref().is_some_and(|n| n.starts_with("First connection")), "{note:?}");
    // Remembered: no question, nothing to say.
    let (_, note) = SftpFs::connect(ask).unwrap();
    assert_eq!(note, None);
    let (_, note) = SftpFs::connect(with_password(c.clone())).unwrap();
    assert_eq!(note, None);
    // Keys: plain and with a passphrase.
    match var("UISON_SFTP_KEY") {
        None => vfs::conformance::skip("UISON_SFTP_KEY"),
        Some(key) => {
            let (s, _) = SftpFs::connect(Config { key: Some(PathBuf::from(key)), ..c.clone() }).unwrap();
            assert!(s.list(&s.home(), &Cancel::new()).is_ok());
        }
    }
    if let (Some(key), Some(pass)) = (var("UISON_SFTP_KEY_PASS"), var("UISON_SFTP_PASSPHRASE")) {
        let ok = Config { key: Some(PathBuf::from(&key)), passphrase: Some(pass), ..c.clone() };
        assert!(SftpFs::connect(ok).is_ok());
        let wrong = Config { key: Some(PathBuf::from(&key)), passphrase: Some("nope".into()), ..c.clone() };
        let e = SftpFs::connect(wrong).err().unwrap().to_string();
        assert!(e.contains(&key), "{e}");
    } else {
        vfs::conformance::skip("UISON_SFTP_KEY_PASS or UISON_SFTP_PASSPHRASE");
    }
    let e = SftpFs::connect(Config { password: Some("wrong".into()), ..c.clone() }).err().unwrap().to_string();
    assert!(e.contains("did not accept the password"), "{e}");
    // Another key for this host in our list: refused, and said why.
    let fake = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGb2Ld5jn5CTqe8NQbuqZdhXjwIoBcb/00CUmnhg6Q9U";
    let host = if c.port == 22 { c.host.clone() } else { format!("[{}]:{}", c.host, c.port) };
    let changed = std::env::temp_dir().join(format!("uison-sftp-changed-{}", stamp()));
    let mut f = vfs_test_file(&changed);
    writeln!(f, "{host} {fake}").unwrap();
    drop(f);
    let e = SftpFs::connect(with_password(Config { known_hosts: changed, ..c })).err().unwrap().to_string();
    assert!(e.contains("CHANGED"), "{e}");
}

/// A file to write the test's known hosts into (tests may touch the disk directly).
fn vfs_test_file(p: &std::path::Path) -> std::fs::File {
    std::fs::File::create(p).unwrap()
}

#[test]
fn copies_both_ways() {
    let Some(c) = config("ops") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let s = Arc::new(s);
    let base = folder(&s, "ops");
    let big: Vec<u8> = (0..20u32 << 20).map(|i| (i % 251) as u8).collect();
    let mem = Arc::new(MemFs::new("m"));
    mem.add_file("/src/big.bin", big.clone()).add_file("/src/tree/a.txt", "a").add_file("/src/tree/sub/b.txt", "bb").add_dir("/back");
    let t = Instant::now();
    let up = Transfer {
        mode: Mode::Copy,
        src: mem.clone(),
        src_dir: VPath::new("/src"),
        names: vec!["big.bin".into(), "tree".into()],
        dst: s.clone(),
        target: base.to_string(),
    };
    let r = up.run(&Quiet, &Cancel::new());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let up_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut got = Vec::new();
    s.open_read(&base.child("big.bin"), 0).unwrap().read_to_end(&mut got).unwrap();
    let down_s = t.elapsed().as_secs_f64();
    assert!(got == big, "{} bytes back of {}", got.len(), big.len());
    eprintln!("20 MB: up {:.1} MB/s, down {:.1} MB/s", 20.0 / up_s, 20.0 / down_s);
    let mut tail = String::new();
    s.open_read(&base.join("tree/sub/b.txt"), 1).unwrap().read_to_string(&mut tail).unwrap();
    assert_eq!(tail, "b");
    // A move to the other source, then a rename on the server.
    let mv =
        Transfer { mode: Mode::Move, src: s.clone(), src_dir: base.clone(), names: vec!["tree".into()], dst: mem.clone(), target: "/back".into() };
    let r = mv.run(&Quiet, &Cancel::new());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(mem.stat(&VPath::new("/back/tree/sub/b.txt")).is_ok() && s.stat(&base.child("tree")).is_err());
    s.rename(&base.child("big.bin"), &base.child("renamed.bin")).unwrap();
    let mode = s.stat(&base.child("renamed.bin")).unwrap().mode.unwrap();
    s.set_mode(&base.child("renamed.bin"), 0o600).unwrap();
    assert_eq!(s.stat(&base.child("renamed.bin")).unwrap().mode, Some(0o600), "was {mode:o}");
    wipe(&s, &base);
}

fn put(s: &SftpFs, p: &VPath, data: &[u8], overwrite: bool) {
    let mut w = s.create(p, &vfs::CreateOpts { overwrite, ..Default::default() }).unwrap();
    w.write_all(data).unwrap();
    w.finish().unwrap();
}

fn get(s: &SftpFs, p: &VPath, offset: u64) -> Vec<u8> {
    let mut out = Vec::new();
    s.open_read(p, offset).unwrap().read_to_end(&mut out).unwrap();
    out
}

/// Overwriting keeps what the file was: its permissions (a private file never opens up on the way), and a
/// link stays a link to the file it pointed to.
#[test]
fn overwriting_keeps_permissions_and_links() {
    let Some(c) = config("over") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let base = folder(&s, "over");
    let f = base.child("secret");
    put(&s, &f, b"old", false);
    s.set_mode(&f, 0o600).unwrap();
    put(&s, &f, b"new", true);
    assert_eq!(get(&s, &f, 0), b"new");
    assert_eq!(s.stat(&f).unwrap().mode, Some(0o600));
    // A link to the file: writing "through" it changes the file, the link stays.
    let link = base.child("link");
    symlink(&s, &f, &link);
    assert!(matches!(s.stat(&link).unwrap().kind, vfs::Kind::Symlink { to_dir: false, .. }), "a link to a file");
    put(&s, &link, b"via link", true);
    assert!(matches!(s.stat(&link).unwrap().kind, vfs::Kind::Symlink { .. }), "still a link");
    assert_eq!(get(&s, &f, 0), b"via link");
    // A link to a folder is entered like a folder.
    s.mkdir(&base.child("dir")).unwrap();
    symlink(&s, &base.child("dir"), &base.child("dlink"));
    let es = s.list(&base, &Cancel::new()).unwrap();
    let dl = es.iter().find(|e| e.name == "dlink").unwrap();
    assert!(matches!(dl.kind, vfs::Kind::Symlink { to_dir: true, .. }) && dl.is_dir(), "{dl:?}");
    s.remove(&base.child("dlink")).unwrap();
    s.remove(&base.child("dir")).unwrap();
    // A name near the 255-byte limit.
    let long = base.child(&format!("{}.txt", "n".repeat(250)));
    put(&s, &long, b"x", false);
    put(&s, &long, b"y", true);
    assert_eq!(get(&s, &long, 0), b"y");
    assert_eq!(s.list(&base, &Cancel::new()).unwrap().len(), 3, "no temporary files left");
    wipe(&s, &base);
}

/// Reads many requests ahead: every offset still gives the right bytes, also past the end.
#[test]
fn reads_ahead_from_any_offset() {
    let Some(c) = config("read") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let base = folder(&s, "read");
    let data: Vec<u8> = (0..3_000_017u32).map(|i| (i % 253) as u8).collect();
    let f = base.child("data");
    put(&s, &f, &data, false);
    for at in [0, 1, 32 << 10, (255 << 10) + 7, 2_999_999, 3_000_017, 3_100_000] {
        let want = &data[(at as usize).min(data.len())..];
        assert!(get(&s, &f, at) == want, "from {at}");
    }
    // A little from the start, as the viewer does, then dropped.
    let mut r = s.open_read(&f, 10).unwrap();
    let mut first = [0u8; 100];
    r.read_exact(&mut first).unwrap();
    assert_eq!(&first[..], &data[10..110]);
    drop(r);
    wipe(&s, &base);
}

/// A folder of one's own under folders that can't be read: named at connect, the panel opens there.
#[test]
fn starts_in_the_folder_named() {
    let Some(c) = config("start") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c.clone())).unwrap();
    let base = folder(&s, "start");
    let mine = base.child("closed").child("mine");
    s.mkdir(&base.child("closed")).unwrap();
    s.mkdir(&mine).unwrap();
    put(&s, &mine.child("a.txt"), b"a", false);
    // Passable, not readable: as /home on a shared hosting.
    s.set_mode(&base.child("closed"), 0o311).unwrap();
    assert!(s.list(&base.child("closed"), &Cancel::new()).is_err(), "the folder above is not readable");
    let rel = mine.as_str().strip_prefix(&format!("{}/", s.home())).unwrap().to_string();
    for typed in [mine.to_string(), rel.clone(), format!("~/{rel}")] {
        let (t, _) = SftpFs::connect(with_password(Config { folder: typed.clone(), ..c.clone() })).unwrap();
        assert_eq!(t.home(), mine, "{typed}");
        assert_eq!(t.list(&t.home(), &Cancel::new()).unwrap().len(), 1);
        assert!(t.roots(&Cancel::new()).unwrap().iter().any(|r| r.path == mine && r.title == mine.as_str()));
    }
    // A folder that isn't there, or a file: said at connect, with the path.
    let missing = SftpFs::connect(with_password(Config { folder: base.child("nope").to_string(), ..c.clone() })).err().unwrap();
    assert!(missing.to_string().contains("nope"), "{missing}");
    let file = SftpFs::connect(with_password(Config { folder: mine.child("a.txt").to_string(), ..c })).err().unwrap();
    assert!(matches!(file, vfs::Error::NotADirectory(_)), "{file}");
    s.set_mode(&base.child("closed"), 0o755).unwrap();
    wipe(&s, &base);
}

/// The server restarts: the next call connects again by itself. Only when asked (it restarts the container):
/// `UISON_SFTP_RESTART="docker restart uison-sshd"`.
#[test]
fn connects_again_after_the_server_restarts() {
    let Some(c) = config("again") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let Some(cmd) = var("UISON_SFTP_RESTART") else { return eprintln!("UISON_SFTP_RESTART is not set: skipped") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let home = s.home();
    assert!(s.list(&home, &Cancel::new()).is_ok());
    assert!(std::process::Command::new("sh").arg("-c").arg(&cmd).status().unwrap().success());
    // The server needs a moment to listen again; the first calls may fail, then it works.
    let t = Instant::now();
    while s.list(&home, &Cancel::new()).is_err() {
        assert!(t.elapsed().as_secs() < 60, "no connection again in 60 s");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// A link on the server (the contract has no links yet).
fn symlink(s: &SftpFs, target: &VPath, link: &VPath) {
    s.make_symlink(target, link).unwrap();
}

/// Throughput alone: one 32 MB file up and down, no per-file round trips around it. For measurements
/// (`--release --nocapture`, through `tools/test-env.sh latency`).
#[test]
fn throughput() {
    let Some(c) = config("speed") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let base = folder(&s, "speed");
    let data: Vec<u8> = (0..32u32 << 20).map(|i| (i % 249) as u8).collect();
    let f = base.child("big");
    let t = Instant::now();
    put(&s, &f, &data, false);
    let up = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let back = get(&s, &f, 0);
    let down = t.elapsed().as_secs_f64();
    assert!(back == data);
    let t = Instant::now();
    s.copy_within(&f, &base.child("copy"), &Cancel::new()).unwrap();
    let copy = t.elapsed().as_secs_f64();
    eprintln!("32 MB alone: up {:.1} MB/s, down {:.1} MB/s, copy on the server {:.0} ms", 32.0 / up, 32.0 / down, copy * 1000.0);
    wipe(&s, &base);
}

/// Many small files: what counts is round trips per file, not bytes. For measurements, as `throughput`.
#[test]
fn small_files() {
    let Some(c) = config("small") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let s = Arc::new(s);
    let base = folder(&s, "small");
    let mem = Arc::new(MemFs::new("m"));
    for i in 0..20 {
        mem.add_file(&format!("/src/many/f{i:02}.txt"), format!("file {i}"));
    }
    let t = Instant::now();
    let up = Transfer {
        mode: Mode::Copy,
        src: mem.clone(),
        src_dir: VPath::new("/src"),
        names: vec!["many".into()],
        dst: s.clone(),
        target: base.to_string(),
    };
    let r = up.run(&Quiet, &Cancel::new());
    assert!(r.errors.is_empty() && r.done == 20, "{r:?}");
    eprintln!("20 small files up: {:.0} ms per file", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
    wipe(&s, &base);
}

/// Esc while a folder is read over a slow link: the listing stops at once, not after the round trip (or
/// the 30 s timeout of a dead one). Sets the test proxy's delay: only with `UISON_LATENCY_TEST=1`.
#[test]
fn a_listing_is_cancelled_in_flight() {
    let Some(c) = config("slow") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    if var("UISON_LATENCY_TEST").is_none() {
        return eprintln!("UISON_LATENCY_TEST is not set: skipped");
    }
    let c = Config { host: "127.0.0.1".into(), port: 2223, ..with_password(c) };
    let (s, _) = SftpFs::connect(c).unwrap();
    let latency = |ms: u32| {
        assert!(std::process::Command::new("tools/test-env.sh").current_dir("../..").args(["latency", &ms.to_string()]).status().unwrap().success())
    };
    latency(1000);
    let cancel = Cancel::new();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        c2.cancel();
    });
    let t = Instant::now();
    let r = s.list(&s.home(), &cancel);
    let took = t.elapsed();
    latency(0);
    assert_eq!(r.err(), Some(vfs::Error::Cancelled));
    assert!(took.as_millis() < 900, "{took:?}");
    // The connection is still good.
    assert!(s.list(&s.home(), &Cancel::new()).is_ok());
}

/// Many files deleted through ops: the REMOVE requests go together.
#[test]
fn many_files_are_deleted_together() {
    let Some(c) = config("del") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    let (s, _) = SftpFs::connect(with_password(c)).unwrap();
    let base = folder(&s, "del");
    let d = base.child("many");
    s.mkdir(&d).unwrap();
    for i in 0..100 {
        put(&s, &d.child(&format!("f{i:03}")), b"x", false);
    }
    let t = Instant::now();
    let r = ops::delete(&s, &base, &["many".into()], &Quiet, &Cancel::new());
    eprintln!("100 files deleted in {} ms", t.elapsed().as_millis());
    assert!(r.errors.is_empty() && r.done == 101, "{r:?}");
    let res = s.remove_files(&[base.child("missing")], &Cancel::new());
    assert!(matches!(res[0], Err(vfs::Error::NotFound(_))), "{res:?}");
    s.remove(&base).unwrap();
}

/// Cancel reaches calls that take no `Cancel` (stat, read, write) through the operation's scope: a copy over
/// a 6 s round trip stops at once, not after the answer. Sets the proxy's delay: `UISON_LATENCY_TEST=1`.
#[test]
fn any_call_stops_on_the_operations_cancel() {
    let Some(c) = config("scope") else { return vfs::conformance::skip("UISON_SFTP_SERVER") };
    if var("UISON_LATENCY_TEST").is_none() {
        return eprintln!("UISON_LATENCY_TEST is not set: skipped");
    }
    let c = Config { host: "127.0.0.1".into(), port: 2223, ..with_password(c) };
    let (s, _) = SftpFs::connect(c).unwrap();
    let s = Arc::new(s);
    let base = folder(&s, "scope");
    put(&s, &base.child("f"), &vec![1u8; 1 << 20], false);
    let latency = |ms: u32| {
        assert!(std::process::Command::new("tools/test-env.sh").current_dir("../..").args(["latency", &ms.to_string()]).status().unwrap().success())
    };
    latency(3000);
    let mem = Arc::new(MemFs::new("m"));
    mem.add_dir("/dst");
    let cancel = Cancel::new();
    let c2 = cancel.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        c2.cancel();
    });
    let t = Instant::now();
    let r = Transfer { mode: Mode::Copy, src: s.clone(), src_dir: base.clone(), names: vec!["f".into()], dst: mem.clone(), target: "/dst".into() }
        .run(&Quiet, &cancel);
    let took = t.elapsed();
    latency(0);
    assert!(r.cancelled, "{r:?}");
    assert!(took.as_millis() < 1500, "stopped at once: {took:?}");
    assert!(mem.paths() == ["/dst"]);
    wipe(&s, &base);
}
