//! The contract as tests: every source must pass them. A new source starts by plugging in here.
//!
//! `run(src, base)` works inside `base` — an existing, empty, writable folder — and panics with a message
//! on the first broken rule.

use std::io::{Read, Write};
use std::time::{Duration, SystemTime};

use crate::*;

/// A test against a real server that has none configured: with `UISON_REQUIRE_NET=1` this is a failure
/// (CI and `tools/test-env.sh` runs must not go green with nothing tested), otherwise it says so and returns.
pub fn skip(what: &str) {
    if std::env::var("UISON_REQUIRE_NET").is_ok_and(|v| v == "1") {
        panic!("{what} is not set, and UISON_REQUIRE_NET=1 requires the network tests");
    }
    eprintln!("{what} is not set: skipped");
}

pub fn run(src: &dyn Source, base: &VPath) {
    let caps = src.info().caps;
    assert!(src.list(base, &Cancel::new()).expect("list base").is_empty(), "base must be empty");
    files(src, base);
    folders(src, base);
    unfinished_write(src, base);
    read_at_offset(src, base);
    odd_names(src, base);
    cancelled_list(src, base);
    if caps.rename {
        rename(src, base, src.case_sensitive(base));
    }
    if caps.set_mtime {
        mtime(src, base);
    }
    if caps.server_copy {
        server_copy(src, base);
    }
}

/// `copy_within` (declared by `server_copy`): copies, replaces an existing target, keeps the time where the
/// source keeps times, leaves nothing else behind; a folder is `Unsupported` (ops walks it).
fn server_copy(src: &dyn Source, base: &VPath) {
    let (a, b) = (base.child("a"), base.child("b"));
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
    write(src, &a, &data, false).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    if src.info().caps.set_mtime {
        src.set_mtime(&a, t).unwrap();
    }
    for _ in 0..2 {
        // The second time over the copy made the first time.
        src.copy_within(&a, &b, &Cancel::new()).expect("server copy");
        assert!(read(src, &b) == data, "copied data");
        if src.info().caps.set_mtime {
            assert_eq!(src.stat(&b).unwrap().modified, Some(t), "copy keeps the time");
        }
    }
    assert_eq!(names(src, base), ["a", "b"], "nothing else left");
    src.mkdir(&base.child("d")).unwrap();
    assert!(matches!(src.copy_within(&base.child("d"), &base.child("d2"), &Cancel::new()), Err(Error::Unsupported)), "a folder");
    for p in ["a", "b", "d"] {
        src.remove(&base.child(p)).unwrap();
    }
}

fn write(src: &dyn Source, p: &VPath, data: &[u8], overwrite: bool) -> Result<()> {
    let mut w = src.create(p, &CreateOpts { overwrite, size_hint: Some(data.len() as u64), ..CreateOpts::default() })?;
    w.write_all(data).map_err(|e| Error::io(p, &e))?;
    w.finish()
}

fn read(src: &dyn Source, p: &VPath) -> Vec<u8> {
    let mut out = Vec::new();
    src.open_read(p, 0).expect("open_read").read_to_end(&mut out).expect("read");
    out
}

fn names(src: &dyn Source, dir: &VPath) -> Vec<String> {
    let mut v: Vec<String> = src.list(dir, &Cancel::new()).expect("list").into_iter().map(|e| e.name).collect();
    v.sort();
    v
}

fn files(src: &dyn Source, base: &VPath) {
    let p = base.child("a.txt");
    write(src, &p, b"hello", false).expect("create a file");
    assert_eq!(read(src, &p), b"hello");
    let e = src.stat(&p).expect("stat");
    assert_eq!((e.name.as_str(), &e.kind, e.size), ("a.txt", &Kind::File, Some(5)));
    assert_eq!(write(src, &p, b"x", false), Err(Error::AlreadyExists(p.clone())), "create without overwrite");
    write(src, &p, b"bye", true).expect("overwrite");
    assert_eq!(read(src, &p), b"bye");
    assert!(matches!(src.stat(&base.child("missing")), Err(Error::NotFound(_))));
    assert!(matches!(src.open_read(&base.child("missing"), 0), Err(Error::NotFound(_))));
    src.remove(&p).expect("remove a file");
    assert!(names(src, base).is_empty());
}

fn folders(src: &dyn Source, base: &VPath) {
    let d = base.child("d");
    src.mkdir(&d).expect("mkdir");
    assert!(matches!(src.mkdir(&d), Err(Error::AlreadyExists(_))), "mkdir of an existing folder");
    assert!(src.stat(&d).expect("stat folder").is_dir());
    assert!(matches!(src.mkdir(&base.join("no/such")), Err(Error::NotFound(_))), "mkdir without the parent");
    write(src, &d.child("f"), b"1", false).expect("file in a folder");
    assert_eq!(names(src, base), ["d"]);
    assert_eq!(names(src, &d), ["f"]);
    assert!(matches!(src.remove(&d), Err(Error::DirNotEmpty(_))), "remove of a non-empty folder");
    assert!(matches!(src.list(&d.child("f"), &Cancel::new()), Err(Error::NotADirectory(_))), "list of a file");
    src.remove(&d.child("f")).unwrap();
    src.remove(&d).expect("remove an empty folder");
    assert!(names(src, base).is_empty());
}

fn unfinished_write(src: &dyn Source, base: &VPath) {
    let p = base.child("partial.bin");
    {
        let mut w = src.create(&p, &CreateOpts::default()).unwrap();
        w.write_all(b"half").unwrap();
        // Dropped without finish.
    }
    assert!(names(src, base).is_empty(), "a write without finish leaves nothing: {:?}", names(src, base));
    // Nor does it spoil an existing file.
    write(src, &p, b"old", false).unwrap();
    {
        let mut w = src.create(&p, &CreateOpts { overwrite: true, ..CreateOpts::default() }).unwrap();
        w.write_all(b"new").unwrap();
    }
    assert_eq!(read(src, &p), b"old", "an unfinished overwrite keeps the old contents");
    src.remove(&p).unwrap();
}

fn read_at_offset(src: &dyn Source, base: &VPath) {
    let p = base.child("digits");
    write(src, &p, b"0123456789", false).unwrap();
    let mut s = String::new();
    src.open_read(&p, 7).unwrap().read_to_string(&mut s).unwrap();
    assert_eq!(s, "789");
    src.remove(&p).unwrap();
}

fn odd_names(src: &dyn Source, base: &VPath) {
    let names_in = ["with space.txt", "проценты %20 и #хеш.md", "emoji 😀", "...", "-dash", ".hidden"];
    for n in names_in {
        write(src, &base.child(n), n.as_bytes(), false).unwrap_or_else(|e| panic!("create '{n}': {e}"));
    }
    let mut want: Vec<String> = names_in.iter().map(|s| s.to_string()).collect();
    want.sort();
    assert_eq!(names(src, base), want);
    for n in names_in {
        assert_eq!(read(src, &base.child(n)), n.as_bytes());
        src.remove(&base.child(n)).unwrap();
    }
    assert!(src.stat(&base.child(".hidden")).is_err());
}

fn cancelled_list(src: &dyn Source, base: &VPath) {
    let c = Cancel::new();
    c.cancel();
    assert_eq!(src.list(base, &c), Err(Error::Cancelled));
}

fn rename(src: &dyn Source, base: &VPath, case_sensitive: bool) {
    let d = base.child("src");
    src.mkdir(&d).unwrap();
    write(src, &d.child("f"), b"1", false).unwrap();
    write(src, &base.child("other"), b"2", false).unwrap();
    let to = base.child("dst");
    src.rename(&d, &to).expect("rename a folder");
    assert_eq!(names(src, base), ["dst", "other"]);
    assert_eq!(read(src, &to.child("f")), b"1");
    assert!(matches!(src.rename(&base.child("other"), &to), Err(Error::AlreadyExists(_))), "rename never replaces");
    assert_eq!(read(src, &base.child("other")), b"2");
    // A change of case only: allowed even where names don't differ by case.
    src.rename(&base.child("other"), &base.child("Other")).expect("case-only rename");
    assert_eq!(names(src, base), ["Other", "dst"]);
    if !case_sensitive {
        assert!(src.stat(&base.child("other")).is_ok(), "case-insensitive source finds by any case");
    }
    src.remove(&base.child("Other")).unwrap();
    src.remove(&to.child("f")).unwrap();
    src.remove(&to).unwrap();
}

fn mtime(src: &dyn Source, base: &VPath) {
    let p = base.child("t");
    write(src, &p, b"", false).unwrap();
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    src.set_mtime(&p, t).unwrap();
    assert_eq!(src.stat(&p).unwrap().modified, Some(t));
    src.remove(&p).unwrap();
    // The time (and permissions) given at create, kept at finish — new and over an existing file.
    let caps = src.info().caps;
    for overwrite in [false, true] {
        let opts = CreateOpts { overwrite, modified: Some(t), mode: caps.permissions.then_some(0o640), ..CreateOpts::default() };
        let mut w = src.create(&p, &opts).unwrap();
        w.write_all(b"timed").unwrap();
        w.finish().unwrap();
        let e = src.stat(&p).unwrap();
        assert_eq!(e.modified, Some(t), "time given at create (overwrite {overwrite})");
        if caps.permissions {
            assert_eq!(e.mode.map(|m| m & 0o777), Some(0o640), "mode given at create (overwrite {overwrite})");
        }
    }
    src.remove(&p).unwrap();
}

#[cfg(test)]
mod tests {
    use crate::MemFs;

    #[test]
    fn memfs_passes() {
        super::run(&MemFs::new("t"), &crate::VPath::root());
    }

    #[test]
    fn memfs_without_rename_passes() {
        super::run(&MemFs::new("t").without_rename(), &crate::VPath::root());
    }
}
