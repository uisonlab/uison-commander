//! Against real WebDAV servers. Skipped unless `UISON_WEBDAV_URL` is set; `UISON_WEBDAV_URL2` is a second
//! server the same tests run on (`tools/test-env.sh`: rclone and Apache, which answer differently):
//!
//! ```text
//! UISON_WEBDAV_URL=http://localhost:8081/ UISON_WEBDAV_URL2=http://localhost:8082/ \
//! UISON_WEBDAV_USER=uison UISON_WEBDAV_PASSWORD=… cargo test -p vfs-webdav
//! ```
//!
//! Every test works in a fresh folder of its own.

use std::io::Read;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ops::{Answer, Feedback, Mode, Progress, Question, Transfer};
use vfs::{Cancel, MemFs, Source, VPath};
use vfs_webdav::{Config, DavFs};

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// The servers to test on; none — skipped.
fn configs() -> Vec<Config> {
    let (user, password) = (var("UISON_WEBDAV_USER").unwrap_or_default(), var("UISON_WEBDAV_PASSWORD").unwrap_or_default());
    ["UISON_WEBDAV_URL", "UISON_WEBDAV_URL2"]
        .iter()
        .filter_map(|k| var(k))
        .map(|url| Config { url, user: user.clone(), password: password.clone() })
        .collect()
}

fn folder(s: &DavFs, tag: &str) -> VPath {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let d = VPath::root().child(&format!("uison-{tag}-{stamp}"));
    s.mkdir(&d).unwrap();
    d
}

fn wipe(s: &DavFs, dir: &VPath) {
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
        return vfs::conformance::skip("UISON_WEBDAV_URL");
    }
    for c in cs {
        eprintln!("{}", c.url);
        f(c);
    }
}

#[test]
fn conformance() {
    on_each(|c| {
        let s = DavFs::connect(c).expect("connect");
        let base = folder(&s, "conf");
        vfs::conformance::run(&s, &base);
        wipe(&s, &base);
    });
}

/// A tree there and back through `ops`: a big file in several chunks, a nested folder, odd names; the
/// listing has sizes and times; a move on the server.
#[test]
fn a_tree_there_and_back() {
    on_each(|c| {
        let dav = Arc::new(DavFs::connect(c).expect("connect"));
        let s: Arc<dyn Source> = dav.clone();
        let mem = MemFs::new("m");
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        mem.add_file("/t/big.bin", big.clone());
        mem.add_file("/t/sub/дерево 1.txt", "один");
        let mem: Arc<dyn Source> = Arc::new(mem);
        let base = folder(&dav, "tree");
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
        assert!(e.modified.is_some(), "the server's time");
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
        wipe(&dav, &base);
    });
}

/// A wrong password and a server that is not there fail at connect, saying so.
#[test]
fn wrong_password_and_no_server() {
    on_each(|c| {
        if !c.user.is_empty() {
            let bad = Config { password: "wrong".into(), ..c.clone() };
            let e = DavFs::connect(bad).err().expect("refused").to_string();
            assert!(e.contains("did not accept"), "{e}");
        }
        let e = DavFs::connect(Config { url: "http://127.0.0.1:1/".into(), ..c }).err().expect("no server").to_string();
        assert!(e.to_lowercase().contains("refused") && e.contains("127.0.0.1:1"), "{e}");
    });
}
