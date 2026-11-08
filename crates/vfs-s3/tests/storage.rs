//! Against a real storage. Skipped unless `UISON_S3_ENDPOINT` is set, e.g. a local MinIO:
//!
//! `UISON_S3_ENDPOINT=http://localhost:9000 UISON_S3_KEY=minioadmin UISON_S3_SECRET=minioadmin cargo test -p vfs-s3`
//!
//! Every test works in a fresh bucket of its own and deletes it at the end.

use std::io::Read;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ops::{Answer, Feedback, Mode, Progress, Question, Transfer};
use vfs::{Cancel, MemFs, Source, VPath};
use vfs_s3::{Config, S3Fs};

fn storage() -> Option<S3Fs> {
    let endpoint = std::env::var("UISON_S3_ENDPOINT").ok()?;
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    Some(S3Fs::new(Config {
        endpoint,
        region: var("UISON_S3_REGION"),
        access_key: var("UISON_S3_KEY"),
        secret_key: var("UISON_S3_SECRET"),
        session_token: None,
        ..Config::default()
    }))
}

/// A new bucket: `/uison-<tag>-<time>`.
fn bucket(s: &S3Fs, tag: &str) -> VPath {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let b = VPath::root().child(&format!("uison-{tag}-{t}"));
    s.mkdir(&b).expect("create a bucket");
    b
}

/// Delete everything under `dir`, then `dir` itself.
fn wipe(s: &S3Fs, dir: &VPath) {
    for e in s.list(dir, &Cancel::new()).unwrap_or_default() {
        let p = dir.child(&e.name);
        if e.is_dir() {
            wipe(s, &p);
        } else {
            s.remove(&p).unwrap();
        }
    }
    s.remove(dir).unwrap();
}

fn put(s: &S3Fs, p: &VPath, data: &[u8]) {
    let mut w = s.create(p, &vfs::CreateOpts::default()).unwrap();
    std::io::Write::write_all(&mut w, data).unwrap();
    w.finish().unwrap();
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
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "conf");
    vfs::conformance::run(&s, &b);
    wipe(&s, &b);
    assert!(!s.list(&VPath::root(), &Cancel::new()).unwrap().iter().any(|e| e.name == b.name()), "the bucket is gone");
}

#[test]
fn copies_both_ways_with_multipart_and_on_the_server() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let s = Arc::new(s);
    let b = bucket(&s, "ops");
    // 20 MB: three parts of a multipart upload.
    let big: Vec<u8> = (0..20u32 << 20).map(|i| (i % 251) as u8).collect();
    let mem = Arc::new(MemFs::new("m"));
    mem.add_file("/src/big.bin", big.clone()).add_file("/src/tree/a.txt", "a").add_file("/src/tree/sub/b.txt", "bb").add_dir("/back");
    let up = Transfer {
        mode: Mode::Copy,
        src: mem.clone(),
        src_dir: VPath::new("/src"),
        names: vec!["big.bin".into(), "tree".into()],
        dst: s.clone(),
        target: b.to_string(),
    };
    let r = up.run(&Quiet, &Cancel::new());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let mut got = Vec::new();
    s.open_read(&b.child("big.bin"), 0).unwrap().read_to_end(&mut got).unwrap();
    assert!(got == big, "multipart upload: {} bytes back of {}", got.len(), big.len());
    let mut tail = String::new();
    s.open_read(&b.join("tree/sub/b.txt"), 1).unwrap().read_to_string(&mut tail).unwrap();
    assert_eq!(tail, "b");
    // Inside the storage: CopyObject, no data through us.
    let same = Transfer {
        mode: Mode::Copy,
        src: s.clone(),
        src_dir: b.clone(),
        names: vec!["tree".into()],
        dst: s.clone(),
        target: b.join("copy").to_string(),
    };
    let r = same.run(&Quiet, &Cancel::new());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let names: Vec<String> = s.list(&b.join("copy/sub"), &Cancel::new()).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["b.txt"]);
    // A move: copy and delete (no rename in S3).
    let mv = Transfer { mode: Mode::Move, src: s.clone(), src_dir: b.clone(), names: vec!["copy".into()], dst: mem.clone(), target: "/back".into() };
    let r = mv.run(&Quiet, &Cancel::new());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(mem.stat(&VPath::new("/back/copy/sub/b.txt")).is_ok());
    assert!(s.stat(&b.join("copy")).is_err(), "moved away");
    // Delete the tree the way F8 does.
    let r = ops::delete(&*s, &b, &["tree".into(), "big.bin".into()], &Quiet, &Cancel::new());
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert!(s.list(&b, &Cancel::new()).unwrap().is_empty());
    s.remove(&b).unwrap();
}

#[test]
fn links_and_actions() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "act");
    let p = b.child("hello world.txt");
    let mut w = s.create(&p, &vfs::CreateOpts::default()).unwrap();
    std::io::Write::write_all(&mut w, b"hi").unwrap();
    w.finish().unwrap();
    let sel = [p.clone()];
    let args = |k: &str, v: &str| vfs::ActionArgs(vec![(k.into(), v.into())]);
    let vfs::ActionResult::Clipboard(signed) = s.run_action("s3.copy-url", &sel, &args("how", "1h"), &Cancel::new()).unwrap() else { panic!() };
    assert!(signed.contains("hello%20world.txt") && signed.contains("X-Amz-Signature="), "{signed}");
    let vfs::ActionResult::Clipboard(public) = s.run_action("s3.copy-url", &sel, &args("how", "public"), &Cancel::new()).unwrap() else { panic!() };
    assert!(public.ends_with(&format!("{}/hello%20world.txt", b.name())), "{public}");
    // The signed link works without the keys; the public one doesn't (the object is private).
    if let Some(body) = http_get(&signed) {
        assert!(body.starts_with("HTTP/1.1 200") && body.ends_with("\r\n\r\nhi"), "{body}");
        let public = http_get(&public).unwrap();
        assert!(public.starts_with("HTTP/1.1 403"), "{public}");
    }
    s.remove(&p).unwrap();
    s.remove(&b).unwrap();
}

/// A plain GET over `http://` (a local storage): the whole answer as text. `None` for `https://`.
fn http_get(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(h, p)| (h, format!("/{p}")));
    let mut c = std::net::TcpStream::connect(host).unwrap();
    std::io::Write::write_all(&mut c, format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).unwrap();
    Some(out)
}

/// The Visibility dialog starts from what is there now: private by default, public when anyone can read
/// (here through a bucket policy, as MinIO has no object ACLs).
#[test]
fn visibility_is_read_before_asking() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "vis");
    let (open, closed) = (b.join("open/a.txt"), b.child("closed.txt"));
    put(&s, &open, b"a");
    put(&s, &closed, b"c");
    let visibility = || s.actions(&b, &[vfs::Entry::new("x", vfs::Kind::File)]).into_iter().find(|a| a.id == "s3.visibility").unwrap();
    let asked = |sel: &[VPath]| match &s.prepare_action(&visibility(), sel, &Cancel::new()).unwrap().params[0] {
        vfs::Param::Choice { title, default, .. } => (title.clone(), default.clone()),
        p => panic!("{p:?}"),
    };
    let (title, default) = asked(std::slice::from_ref(&closed));
    assert_eq!((title.as_str(), default.as_str()), ("Who can read — now: only the owner", "private"));
    // Anyone may read `open/*`.
    let policy = format!(
        r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Principal":{{"AWS":["*"]}},"Action":["s3:GetObject"],"Resource":["arn:aws:s3:::{}/open/*"]}}]}}"#,
        b.name()
    );
    put_policy(b.name(), &policy);
    let (title, default) = asked(std::slice::from_ref(&open));
    assert!(title.starts_with("Who can read — now: everyone") && title.contains("bucket policy"), "{title}");
    assert_eq!(default, "public-read");
    let (title, default) = asked(&[open.clone(), closed.clone()]);
    assert_eq!(
        (title.as_str(), default.as_str()),
        ("Who can read — now: 1 public, 1 private. Public through the bucket policy: the ACL can't hide it", "private")
    );
    s.remove(&open).unwrap();
    s.remove(&closed).unwrap();
    s.remove(&b).unwrap();
}

/// A bucket policy, straight through the SDK: the source itself has no use for one.
fn put_policy(bucket: &str, policy: &str) {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _g = rt.enter();
    let cfg = aws_sdk_s3::config::Builder::new()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new(if var("UISON_S3_REGION").is_empty() { "us-east-1".to_string() } else { var("UISON_S3_REGION") }))
        .credentials_provider(Credentials::new(var("UISON_S3_KEY"), var("UISON_S3_SECRET"), None, None, "test"))
        .endpoint_url(var("UISON_S3_ENDPOINT"))
        .force_path_style(true)
        .http_client(aws_smithy_http_client::Builder::new().tls_provider(Provider::Rustls(CryptoMode::Ring)).build_https())
        .build();
    let c = aws_sdk_s3::Client::from_conf(cfg);
    rt.block_on(c.put_bucket_policy().bucket(bucket).policy(policy).send()).expect("put the bucket policy");
}

/// HTTPS to the real Amazon S3, with wrong keys: the answer must be "permission denied" — proof that TLS
/// (rustls on ring, the system's root certificates) works. Needs the internet: `UISON_S3_TLS_CHECK=1`.
#[test]
fn https_reaches_amazon() {
    if std::env::var("UISON_S3_TLS_CHECK").is_err() {
        return eprintln!("UISON_S3_TLS_CHECK is not set: skipped");
    }
    let s = S3Fs::new(Config {
        endpoint: String::new(),
        region: "us-east-1".into(),
        access_key: "AKIAEXAMPLE".into(),
        secret_key: "wrong".into(),
        session_token: None,
        ..Config::default()
    });
    let e = s.list(&VPath::root(), &Cancel::new()).expect_err("wrong keys can't list buckets");
    assert!(matches!(e, vfs::Error::PermissionDenied(_)) || e.to_string().contains("InvalidAccessKeyId"), "{e}");
}

/// The raw SDK client for things the source doesn't do (odd keys, multipart listings, headers).
fn raw() -> (tokio::runtime::Runtime, aws_sdk_s3::Client) {
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
    let var = |k: &str| std::env::var(k).unwrap_or_default();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cfg = {
        let _g = rt.enter();
        aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new(var("UISON_S3_KEY"), var("UISON_S3_SECRET"), None, None, "test"))
            .endpoint_url(var("UISON_S3_ENDPOINT"))
            .force_path_style(true)
            .http_client(aws_smithy_http_client::Builder::new().tls_provider(Provider::Rustls(CryptoMode::Ring)).build_https())
            .build()
    };
    (rt, aws_sdk_s3::Client::from_conf(cfg))
}

/// Keys a path can't name (`./`, `../`, `//`) and a control character: the folder still lists, and shows
/// what it can. Uploads get a Content-Type, so links open in the browser.
#[test]
fn odd_keys_and_content_types() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "odd");
    let (rt, c) = raw();
    let name = b.name().to_string();
    // MinIO refuses `.` and `..` itself; Amazon takes them. Whatever is taken must not break the listing.
    let mut taken = Vec::new();
    for k in ["d/./x", "d/../y", "d//z", "d/ctl\u{1}name", "d/ok.html"] {
        if rt.block_on(c.put_object().bucket(&name).key(k).body(aws_sdk_s3::primitives::ByteStream::from_static(b"x")).send()).is_ok() {
            taken.push(k);
        }
    }
    let mut names: Vec<String> = s.list(&b.child("d"), &Cancel::new()).unwrap().into_iter().map(|e| e.name).collect();
    names.sort();
    let want: Vec<&str> = ["ctl\u{1}name", "ok.html"].into_iter().filter(|n| taken.contains(&format!("d/{n}").as_str())).collect();
    assert_eq!(names, want, "taken: {taken:?}");
    put(&s, &b.child("page.html"), b"<b>hi</b>");
    let ct = rt.block_on(c.head_object().bucket(&name).key("page.html").send()).unwrap().content_type().map(str::to_string);
    assert_eq!(ct.as_deref(), Some("text/html; charset=utf-8"));
    for k in taken.into_iter().chain(["page.html"]) {
        rt.block_on(c.delete_object().bucket(&name).key(k).send()).unwrap();
    }
    s.remove(&b).unwrap();
}

/// A big upload dropped half way leaves no multipart upload behind (they are billed); a whole one is
/// right byte for byte, its parts sent several at once.
#[test]
fn multipart_aborts_and_completes() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "multi");
    let (rt, c) = raw();
    let name = b.name().to_string();
    let data: Vec<u8> = (0..(40u32 << 20)).map(|i| (i % 239) as u8).collect();
    let mut w = s.create(&b.child("half"), &vfs::CreateOpts { size_hint: Some(data.len() as u64), ..Default::default() }).unwrap();
    std::io::Write::write_all(&mut w, &data[..20 << 20]).unwrap();
    drop(w);
    // The abort is done when drop returns; the storage may list it a moment longer (MinIO under load did).
    let t = std::time::Instant::now();
    loop {
        let open = rt.block_on(c.list_multipart_uploads().bucket(&name).send()).unwrap();
        if open.uploads().is_empty() {
            break;
        }
        assert!(t.elapsed().as_secs() < 3, "{:?}", open.uploads());
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let t = std::time::Instant::now();
    put(&s, &b.child("whole"), &data);
    eprintln!("40 MB up: {:.0} MB/s", 40.0 / t.elapsed().as_secs_f64());
    let mut back = Vec::new();
    s.open_read(&b.child("whole"), 0).unwrap().read_to_end(&mut back).unwrap();
    assert!(back == data);
    wipe(&s, &b);
}

/// A folder of many files deleted through ops: a few DeleteObjects requests instead of two requests a file.
#[test]
fn many_files_are_deleted_in_batches() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "batch");
    let d = b.child("many");
    for i in 0..120 {
        put(&s, &d.child(&format!("f{i:03}")), b"x");
    }
    put(&s, &d.join("sub/inner"), b"y");
    let t = std::time::Instant::now();
    let r = ops::delete(&s, &b, &["many".into()], &Quiet, &Cancel::new());
    eprintln!("121 objects deleted in {} ms", t.elapsed().as_millis());
    assert!(r.errors.is_empty() && r.done >= 121, "{r:?}");
    assert!(s.list(&b, &Cancel::new()).unwrap().is_empty());
    // Straight: a path outside any bucket is an error of its own, the others go.
    put(&s, &b.child("one"), b"1");
    let res = s.remove_files(&[b.child("one"), VPath::new("/")], &Cancel::new());
    assert!(res[0].is_ok() && res[1].is_err(), "{res:?}");
    s.remove(&b).unwrap();
}

/// An object larger than one CopyObject takes is copied in parts on the storage (UploadPartCopy). The limit
/// is lowered to 10 MB for the check (`UISON_S3_COPY_MAX`), only in this test's process run of it.
#[test]
fn big_objects_are_copied_in_parts() {
    let Some(s) = storage() else { return vfs::conformance::skip("UISON_S3_ENDPOINT") };
    let b = bucket(&s, "bigcopy");
    let data: Vec<u8> = (0..(23u32 << 20)).map(|i| (i % 233) as u8).collect();
    put(&s, &b.child("big"), &data);
    // SAFETY of the env: only this test reads the variable, and only in copy_within below.
    unsafe { std::env::set_var("UISON_S3_COPY_MAX", (10u64 << 20).to_string()) };
    let r = s.copy_within(&b.child("big"), &b.child("copy"), &Cancel::new());
    unsafe { std::env::remove_var("UISON_S3_COPY_MAX") };
    r.unwrap();
    let mut back = Vec::new();
    s.open_read(&b.child("copy"), 0).unwrap().read_to_end(&mut back).unwrap();
    assert!(back == data, "{} bytes back of {}", back.len(), data.len());
    wipe(&s, &b);
}
