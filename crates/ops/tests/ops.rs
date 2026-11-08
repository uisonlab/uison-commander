//! Operations on `MemFs` (two different sources, one source, without rename) and on the local disk.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ops::*;
use vfs::{Cancel, MemFs, Source, VPath};

/// Answers questions from a script, records them, can cancel at a byte count.
#[derive(Default)]
struct Script {
    answers: Mutex<Vec<Answer>>,
    asked: Mutex<Vec<Question>>,
    last: Mutex<Progress>,
    cancel_at: Option<(u64, Cancel)>,
    on_ask: Option<Box<dyn Fn() + Sync>>,
}

impl Script {
    fn answers(a: &[Answer]) -> Script {
        Script { answers: Mutex::new(a.iter().rev().cloned().collect()), ..Script::default() }
    }

    fn asked(&self) -> Vec<Question> {
        self.asked.lock().unwrap().clone()
    }
}

impl Feedback for Script {
    fn progress(&self, p: &Progress) {
        *self.last.lock().unwrap() = p.clone();
        if let Some((at, c)) = &self.cancel_at
            && !p.planning
            && p.bytes_done >= *at
        {
            c.cancel();
        }
    }

    fn ask(&self, q: Question) -> Answer {
        self.asked.lock().unwrap().push(q);
        if let Some(f) = &self.on_ask {
            f();
        }
        self.answers.lock().unwrap().pop().expect("an unexpected question")
    }
}

fn tree(fs: &MemFs) {
    fs.add_file("/src/a.txt", "aaa").add_file("/src/dir/b.txt", "bb").add_file("/src/dir/sub/c.txt", "c").add_dir("/src/empty").add_dir("/dst");
}

fn t(src: &Arc<MemFs>, dst: &Arc<MemFs>, mode: Mode, names: &[&str], target: &str) -> Transfer {
    Transfer {
        mode,
        src: src.clone(),
        src_dir: VPath::new("/src"),
        names: names.iter().map(|s| s.to_string()).collect(),
        dst: dst.clone(),
        target: target.into(),
    }
}

fn ok(r: &Report) {
    assert!(r.errors.is_empty() && !r.cancelled && r.failed == 0, "{r:?}");
}

#[test]
fn copy_between_sources_keeps_tree_and_dates() {
    let (a, b) = (Arc::new(MemFs::new("a")), Arc::new(MemFs::new("b")));
    tree(&a);
    b.add_dir("/dst");
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    a.set_modified("/src/a.txt", old);
    let fb = Script::default();
    let r = t(&a, &b, Mode::Copy, &["a.txt", "dir", "empty"], "/dst").run(&fb, &Cancel::new());
    ok(&r);
    assert_eq!(r.done, 3);
    assert_eq!(b.paths(), ["/dst", "/dst/a.txt", "/dst/dir", "/dst/dir/b.txt", "/dst/dir/sub", "/dst/dir/sub/c.txt", "/dst/empty"]);
    assert_eq!(b.read("/dst/dir/sub/c.txt").unwrap(), b"c");
    assert_eq!(b.stat(&VPath::new("/dst/a.txt")).unwrap().modified, Some(old));
    assert!(a.exists("/src/dir/sub/c.txt"), "copy keeps the source");
    let p = fb.last.lock().unwrap().clone();
    assert_eq!((p.files_done, p.files_total, p.bytes_done, p.bytes_total), (3, 3, 6, 6));
}

#[test]
fn move_inside_a_source_is_a_rename() {
    let a = Arc::new(MemFs::new("a").slow(0));
    tree(&a);
    let r = t(&a, &a, Mode::Move, &["dir", "a.txt"], "/dst/").run(&Script::default(), &Cancel::new());
    ok(&r);
    assert_eq!(r.done, 2);
    assert!(!a.exists("/src/dir") && a.read("/dst/dir/sub/c.txt").unwrap() == b"c" && a.exists("/dst/a.txt"));
}

#[test]
fn move_without_rename_copies_then_deletes() {
    for (a, b) in [(Arc::new(MemFs::new("a").without_rename()), None), (Arc::new(MemFs::new("a")), Some(Arc::new(MemFs::new("b"))))] {
        tree(&a);
        let b = b.unwrap_or_else(|| a.clone());
        b.add_dir("/dst");
        let r = t(&a, &b, Mode::Move, &["dir", "a.txt"], "/dst").run(&Script::default(), &Cancel::new());
        ok(&r);
        assert!(!a.exists("/src/dir") && !a.exists("/src/a.txt"), "{:?}", a.paths());
        assert_eq!(b.read("/dst/dir/sub/c.txt").unwrap(), b"c");
    }
}

#[test]
fn conflicts() {
    let a = Arc::new(MemFs::new("a"));
    let b = Arc::new(MemFs::new("b"));
    for n in ["1", "2", "3", "4"] {
        a.add_file(&format!("/src/{n}"), format!("new{n}"));
        b.add_file(&format!("/dst/{n}"), format!("old{n}"));
    }
    let names = ["1", "2", "3", "4"];
    // Overwrite, Skip, Rename, then Overwrite all takes the 4th without asking.
    let fb = Script::answers(&[Answer::Overwrite, Answer::Skip, Answer::Rename("3b".into()), Answer::OverwriteAll]);
    let r = t(&a, &b, Mode::Copy, &names, "/dst").run(&fb, &Cancel::new());
    assert_eq!((r.done, r.skipped), (3, 1));
    assert_eq!(fb.asked().len(), 4);
    let rd = |p: &str| String::from_utf8(b.read(p).unwrap()).unwrap();
    assert_eq!([rd("/dst/1"), rd("/dst/2"), rd("/dst/3"), rd("/dst/3b"), rd("/dst/4")], ["new1", "old2", "old3", "new3", "new4"]);
    // Skip all: asked once.
    let fb = Script::answers(&[Answer::SkipAll]);
    let r = t(&a, &b, Mode::Copy, &names, "/dst").run(&fb, &Cancel::new());
    assert_eq!((r.done, r.skipped, fb.asked().len()), (0, 4, 1));
    // Overwrite older: only targets older than the source.
    let past = SystemTime::now() - Duration::from_secs(3600);
    let future = SystemTime::now() + Duration::from_secs(3600);
    b.set_modified("/dst/1", past);
    b.set_modified("/dst/2", future);
    b.add_file("/dst/2", "keep2");
    b.set_modified("/dst/2", future);
    let fb = Script::answers(&[Answer::OverwriteOlder]);
    let r = t(&a, &b, Mode::Copy, &["1", "2"], "/dst").run(&fb, &Cancel::new());
    assert_eq!((r.done, r.skipped, fb.asked().len()), (1, 1, 1));
    assert_eq!(rd("/dst/2"), "keep2");
    // Cancel.
    let fb = Script::answers(&[Answer::Cancel]);
    let r = t(&a, &b, Mode::Copy, &names, "/dst").run(&fb, &Cancel::new());
    assert!(r.cancelled);
}

#[test]
fn move_over_existing_files_merges_folders() {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/src/d/x", "new").add_file("/src/d/y", "y").add_file("/dst/d/x", "old");
    let r = t(&a, &a, Mode::Move, &["d"], "/dst").run(&Script::answers(&[Answer::Overwrite]), &Cancel::new());
    ok(&r);
    assert_eq!(a.paths(), ["/dst", "/dst/d", "/dst/d/x", "/dst/d/y", "/src"]);
    assert_eq!(a.read("/dst/d/x").unwrap(), b"new");
}

#[test]
fn cancel_in_the_middle_leaves_no_partial_file() {
    // Slow reads: the progress (sent at most every 50 ms) sees the middle of the file.
    let a = Arc::new(MemFs::new("a").slow(30));
    let b = Arc::new(MemFs::new("b"));
    a.add_file("/src/big", vec![7u8; 5 * BLOCK]);
    b.add_dir("/dst");
    let c = Cancel::new();
    let fb = Script { cancel_at: Some((2 * BLOCK as u64, c.clone())), ..Script::default() };
    let r = t(&a, &b, Mode::Copy, &["big"], "/dst").run(&fb, &c);
    assert!(r.cancelled);
    let done = fb.last.lock().unwrap().bytes_done;
    assert!(done > 0 && done < 5 * BLOCK as u64, "cancelled in the middle, not at {done}");
    assert_eq!(b.paths(), ["/dst"]);
    assert_eq!(b.open_writers(), 0, "the unfinished write is dropped");
}

#[test]
fn errors_retry_skip() {
    let a = Arc::new(MemFs::new("a"));
    let b = Arc::new(MemFs::new("b"));
    a.add_file("/src/1", "1").add_file("/src/2", "2").add_file("/src/3", "3");
    b.add_dir("/dst");
    a.fail_on("/src/1");
    a.fail_on("/src/2");
    // The "user" heals the failure while the question is open: "1" is skipped anyway, "2" no longer fails.
    let healer = a.clone();
    let fb = Script { on_ask: Some(Box::new(move || healer.heal())), ..Script::answers(&[Answer::Skip]) };
    let r = t(&a, &b, Mode::Copy, &["1", "2", "3"], "/dst").run(&fb, &Cancel::new());
    assert_eq!((r.done, r.failed, fb.asked().len()), (2, 1, 1), "{r:?}");
    assert_eq!(b.paths(), ["/dst", "/dst/2", "/dst/3"]);
    // Retry after a heal copies it.
    a.fail_on("/src/1");
    let healer = a.clone();
    let fb = Script { on_ask: Some(Box::new(move || healer.heal())), ..Script::answers(&[Answer::Retry]) };
    let r = t(&a, &b, Mode::Copy, &["1"], "/dst").run(&fb, &Cancel::new());
    ok(&r);
    assert_eq!(b.read("/dst/1").unwrap(), b"1");
}

#[test]
fn checks_before_start() {
    let a = Arc::new(MemFs::new("a"));
    tree(&a);
    let before = a.paths();
    let r = t(&a, &a, Mode::Copy, &["dir"], "/src/dir/sub").run(&Script::default(), &Cancel::new());
    assert!(r.errors[0].contains("into itself"), "{r:?}");
    let r = t(&a, &a, Mode::Move, &["dir"], "/src/dir/sub").run(&Script::default(), &Cancel::new());
    assert!(r.errors[0].contains("into itself"), "{r:?}");
    let r = t(&a, &a, Mode::Copy, &["a.txt"], "/src").run(&Script::default(), &Cancel::new());
    assert!(r.errors[0].contains("onto itself"), "{r:?}");
    assert_eq!(a.paths(), before);
    let ro = Arc::new(MemFs::new("ro").read_only());
    let r = t(&a, &ro, Mode::Copy, &["a.txt"], "/").run(&Script::default(), &Cancel::new());
    assert!(r.errors[0].contains("read-only"), "{r:?}");
}

#[test]
fn targets_rename_mask_and_new_folder() {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/src/x.txt", "x").add_file("/src/y.txt", "y");
    // One object to a name that is not a folder: rename.
    ok(&t(&a, &a, Mode::Move, &["x.txt"], "/src/z.md").run(&Script::default(), &Cancel::new()));
    assert!(a.exists("/src/z.md") && !a.exists("/src/x.txt"));
    // A mask.
    ok(&t(&a, &a, Mode::Copy, &["z.md", "y.txt"], "/src/*.bak").run(&Script::default(), &Cancel::new()));
    assert!(a.exists("/src/z.bak") && a.exists("/src/y.bak"));
    // Several objects to a path that doesn't exist: a new folder.
    ok(&t(&a, &a, Mode::Copy, &["z.md", "y.txt"], "/new/deep").run(&Script::default(), &Cancel::new()));
    assert!(a.exists("/new/deep/z.md") && a.exists("/new/deep/y.txt"));
}

#[test]
fn delete_tree_and_keep_what_failed() {
    let a = MemFs::new("a");
    tree(&a);
    a.fail_on("/src/dir/sub/c.txt");
    let fb = Script::answers(&[Answer::Skip]);
    let r = delete(&a, &VPath::new("/src"), &["dir".into(), "a.txt".into()], &fb, &Cancel::new());
    assert_eq!((r.done, r.failed, r.skipped), (2, 1, 2), "{r:?}");
    assert_eq!(a.paths(), ["/dst", "/src", "/src/dir", "/src/dir/sub", "/src/dir/sub/c.txt", "/src/empty"]);
    a.heal();
    let r = delete(&a, &VPath::new("/src"), &["dir".into(), "empty".into()], &Script::default(), &Cancel::new());
    ok(&r);
    assert_eq!(a.paths(), ["/dst", "/src"]);
}

#[test]
fn mkdir_levels_and_sizes() {
    let a = MemFs::new("a");
    tree(&a);
    assert_eq!(mkdir(&a, &VPath::new("/dst"), "x/y/z").unwrap(), VPath::new("/dst/x"));
    assert!(a.exists("/dst/x/y/z"));
    assert!(mkdir(&a, &VPath::new("/dst"), "x/y/z").is_err(), "the last level exists");
    assert!(mkdir(&a, &VPath::new("/dst"), "../escape").is_err());
    let s = dir_size(&a, &VPath::new("/src"), &Cancel::new()).unwrap();
    assert_eq!(s, Size { bytes: 6, files: 3, dirs: 3 });
}

#[test]
fn new_file_is_empty_and_never_replaces() {
    let a = MemFs::new("a");
    tree(&a);
    assert_eq!(new_file(&a, &VPath::new("/dst"), " notes.md ").unwrap(), VPath::new("/dst/notes.md"));
    assert_eq!(a.stat(&VPath::new("/dst/notes.md")).unwrap().size, Some(0));
    a.add_file("/dst/kept.txt", "data");
    assert!(matches!(new_file(&a, &VPath::new("/dst"), "kept.txt"), Err(vfs::Error::AlreadyExists(_))));
    assert_eq!(a.stat(&VPath::new("/dst/kept.txt")).unwrap().size, Some(4), "not emptied");
    for bad in ["", "a/b", "..", "."] {
        assert!(new_file(&a, &VPath::new("/dst"), bad).is_err(), "{bad:?}");
    }
}

/// The local disk: clone inside a volume, a tree move by rename, a cancelled stream leaves no temp files.
#[test]
fn local_disk() {
    use std::fs;
    let dir = std::env::temp_dir().join(format!("uc-ops-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src/d")).unwrap();
    fs::create_dir_all(dir.join("dst")).unwrap();
    fs::write(dir.join("src/d/f"), vec![1u8; 3 * BLOCK + 5]).unwrap();
    fs::write(dir.join("src/g"), b"g").unwrap();
    let dir = dir.canonicalize().unwrap();
    let base = vfs_local::vpath(&dir);
    let local: Arc<dyn Source> = Arc::new(vfs_local::LocalFs::new());
    let tr = |mode, names: &[&str], target: VPath| Transfer {
        mode,
        src: local.clone(),
        src_dir: base.child("src"),
        names: names.iter().map(|s| s.to_string()).collect(),
        dst: local.clone(),
        target: target.to_string(),
    };
    ok(&tr(Mode::Copy, &["d", "g"], base.child("dst")).run(&Script::default(), &Cancel::new()));
    assert_eq!(fs::read(dir.join("dst/d/f")).unwrap().len(), 3 * BLOCK + 5);
    // Into a memory source: streamed; cancelled on the first progress of the file (after the planning).
    let mem = Arc::new(MemFs::new("m"));
    mem.add_dir("/x");
    let c = Cancel::new();
    let fb = Script { cancel_at: Some((0, c.clone())), ..Script::default() };
    let r = Transfer {
        mode: Mode::Copy,
        src: local.clone(),
        src_dir: base.join("src/d"),
        names: vec!["f".into()],
        dst: mem.clone(),
        target: "/x".into(),
    }
    .run(&fb, &c);
    assert!(r.cancelled && mem.paths() == ["/x"] && mem.open_writers() == 0, "{r:?}");
    // And back from memory to disk, cancelled: no temp file in the target folder.
    let slow = Arc::new(MemFs::new("slow").slow(30));
    slow.add_file("/x/big", vec![0u8; 4 * BLOCK]);
    let c = Cancel::new();
    let fb = Script { cancel_at: Some((2 * BLOCK as u64, c.clone())), ..Script::default() };
    let r = Transfer {
        mode: Mode::Copy,
        src: slow.clone(),
        src_dir: VPath::new("/x"),
        names: vec!["big".into()],
        dst: local.clone(),
        target: base.child("dst").to_string(),
    }
    .run(&fb, &c);
    assert!(r.cancelled);
    let mut left: Vec<String> = fs::read_dir(dir.join("dst")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, ["d", "g"]);
    // A move of a tree inside the volume: one rename.
    ok(&tr(Mode::Move, &["d"], base.join("dst/moved")).run(&Script::default(), &Cancel::new()));
    assert!(dir.join("dst/moved/f").exists() && !dir.join("src/d").exists());
    fs::remove_dir_all(dir).unwrap();
}

// ---- Things that once lost data or hung (Task 2, stage 1) ----

/// A fresh folder on the real disk, links resolved (`/var` → `/private/var` on macOS).
fn scratch(tag: &str) -> (std::path::PathBuf, VPath) {
    let dir = std::env::temp_dir().join(format!("uc-ops-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let v = vfs_local::vpath(&dir);
    (dir, v)
}

fn local() -> Arc<dyn Source> {
    Arc::new(vfs_local::LocalFs::new())
}

fn transfer(mode: Mode, src: &Arc<dyn Source>, dir: VPath, names: &[&str], dst: &Arc<dyn Source>, target: &str) -> Transfer {
    Transfer { mode, src: src.clone(), src_dir: dir, names: names.iter().map(|s| s.to_string()).collect(), dst: dst.clone(), target: target.into() }
}

/// `alias` is a link to `src`: `alias/f` is `src/f`. Moving `src/f` onto it with "Overwrite" once deleted the
/// "existing target" — the file itself.
#[cfg(unix)]
#[test]
fn moving_a_file_onto_itself_through_a_link_keeps_it() {
    let (dir, base) = scratch("alias");
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/f"), b"data").unwrap();
    std::os::unix::fs::symlink(dir.join("src"), dir.join("alias")).unwrap();
    let l = local();
    for mode in [Mode::Move, Mode::Copy] {
        let fb = Script::answers(&[Answer::Overwrite]);
        let r = transfer(mode, &l, base.child("src"), &["f"], &l, &format!("{}/", base.child("alias"))).run(&fb, &Cancel::new());
        assert_eq!(std::fs::read(dir.join("src/f")).unwrap(), b"data", "{mode:?}: {r:?}");
        assert!(fb.asked().is_empty(), "{mode:?}: no question about overwriting a file with itself");
        assert!(r.done == 0 && (r.skipped == 1 || r.failed == 1), "{mode:?}: {r:?}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

/// A selected link to a folder, moved to another source: the folder's contents are copied, then only the
/// link goes — the folder it points to stays whole.
#[cfg(unix)]
#[test]
fn moving_a_link_to_a_folder_elsewhere_keeps_the_folder() {
    let (dir, base) = scratch("linkmove");
    std::fs::create_dir_all(dir.join("real/sub")).unwrap();
    std::fs::write(dir.join("real/a"), b"a").unwrap();
    std::fs::write(dir.join("real/sub/b"), b"b").unwrap();
    std::fs::create_dir(dir.join("src")).unwrap();
    std::os::unix::fs::symlink(dir.join("real"), dir.join("src/link")).unwrap();
    let mem = Arc::new(MemFs::new("m"));
    mem.add_dir("/dst");
    let m: Arc<dyn Source> = mem.clone();
    let r = transfer(Mode::Move, &local(), base.child("src"), &["link"], &m, "/dst").run(&Script::default(), &Cancel::new());
    ok(&r);
    assert_eq!(mem.read("/dst/link/sub/b").unwrap(), b"b");
    assert!(dir.join("real/a").exists() && dir.join("real/sub/b").exists(), "the folder behind the link is untouched");
    assert!(std::fs::symlink_metadata(dir.join("src/link")).is_err(), "the link itself is moved away");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A named pipe is not a file: opening it waits for a writer forever. It is skipped and said so.
#[cfg(unix)]
#[test]
fn a_pipe_is_skipped_not_waited_for() {
    let (dir, base) = scratch("fifo");
    std::fs::create_dir(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/f"), b"f").unwrap();
    assert!(std::process::Command::new("mkfifo").arg(dir.join("src/pipe")).status().unwrap().success());
    let mem = Arc::new(MemFs::new("m"));
    let m: Arc<dyn Source> = mem.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let src = base.clone();
    std::thread::spawn(move || {
        let r = transfer(Mode::Copy, &local(), src, &["src"], &m, "/").run(&Script::default(), &Cancel::new());
        let _ = tx.send(r);
    });
    let r = rx.recv_timeout(Duration::from_secs(10)).expect("the copy hung on the pipe");
    assert_eq!(mem.read("/src/f").unwrap(), b"f");
    assert!(!mem.exists("/src/pipe"));
    assert!(r.skipped == 1 && r.errors.iter().any(|e| e.contains("pipe")), "{r:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A name near the 255-byte limit: the temporary name next to it must not be longer.
#[test]
fn long_names_copy_to_disk() {
    let (dir, base) = scratch("long");
    let name = format!("{}.txt", "n".repeat(250));
    let mem = Arc::new(MemFs::new("m"));
    mem.add_file(&format!("/src/{name}"), "x");
    let m: Arc<dyn Source> = mem.clone();
    let r = transfer(Mode::Copy, &m, VPath::new("/src"), &[&name], &local(), &base.to_string()).run(&Script::default(), &Cancel::new());
    ok(&r);
    assert_eq!(std::fs::read(dir.join(&name)).unwrap(), b"x");
    // And a clone inside the disk.
    let l = local();
    let r = transfer(Mode::Copy, &l, base.clone(), &[&name], &l, &format!("{base}/copy/")).run(&Script::default(), &Cancel::new());
    ok(&r);
    assert_eq!(std::fs::read(dir.join("copy").join(&name)).unwrap(), b"x");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A source where a file appears under the target name right when the copy creates it (another program).
struct Racy(Arc<MemFs>);

impl Source for Racy {
    fn info(&self) -> vfs::SourceInfo {
        self.0.info()
    }
    fn roots(&self, c: &Cancel) -> vfs::Result<Vec<vfs::Root>> {
        self.0.roots(c)
    }
    fn home(&self) -> VPath {
        self.0.home()
    }
    fn list(&self, d: &VPath, c: &Cancel) -> vfs::Result<Vec<vfs::Entry>> {
        self.0.list(d, c)
    }
    fn stat(&self, p: &VPath) -> vfs::Result<vfs::Entry> {
        self.0.stat(p)
    }
    fn open_read(&self, p: &VPath, o: u64) -> vfs::Result<Box<dyn std::io::Read + Send>> {
        self.0.open_read(p, o)
    }
    fn create(&self, p: &VPath, o: &vfs::CreateOpts) -> vfs::Result<Box<dyn vfs::Writer>> {
        if !self.0.exists(p.as_str()) {
            self.0.add_file(p.as_str(), "theirs");
        }
        self.0.create(p, o)
    }
    fn mkdir(&self, p: &VPath) -> vfs::Result<()> {
        self.0.mkdir(p)
    }
    fn remove(&self, p: &VPath) -> vfs::Result<()> {
        self.0.remove(p)
    }
}

#[test]
fn a_file_that_appears_meanwhile_is_not_overwritten_without_asking() {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/src/f", "ours");
    let b = Arc::new(MemFs::new("b"));
    b.add_dir("/dst");
    let racy: Arc<dyn Source> = Arc::new(Racy(b.clone()));
    let fb = Script::answers(&[Answer::Skip]);
    let r = Transfer { mode: Mode::Copy, src: a, src_dir: VPath::new("/src"), names: vec!["f".into()], dst: racy, target: "/dst".into() }
        .run(&fb, &Cancel::new());
    assert_eq!(b.read("/dst/f").unwrap(), b"theirs", "{r:?}");
    assert!(matches!(fb.asked()[..], [Question::Exists { .. }]), "{:?}", fb.asked());
}

/// "Overwrite" on a move: if the move then fails, the old target must still be there.
#[test]
fn a_failed_move_keeps_the_old_target() {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/src/f", "new").add_file("/dst/f", "old");
    a.fail_on("/src/f");
    let fb = Script::answers(&[Answer::Overwrite, Answer::Skip]);
    let r = t(&a, &a, Mode::Move, &["f"], "/dst").run(&fb, &Cancel::new());
    assert_eq!(r.failed, 1, "{r:?}");
    assert_eq!(a.read("/dst/f").unwrap(), b"old");
    assert_eq!(a.read("/src/f").unwrap(), b"new");
    assert_eq!(a.paths(), ["/dst", "/dst/f", "/src", "/src/f"], "no leftovers");
}

/// On a volume that ignores case, `A.txt` is `a.txt`: copying one onto the other is copying onto itself.
#[test]
fn copy_onto_itself_in_another_case() {
    let a = Arc::new(MemFs::new("a").ignore_case());
    a.add_file("/src/a.txt", "a");
    let fb = Script::default();
    let r = t(&a, &a, Mode::Copy, &["a.txt"], "/src/A.txt").run(&fb, &Cancel::new());
    assert!(r.errors.first().is_some_and(|e| e.contains("onto itself")), "{r:?}");
    // A move to another case is a rename.
    ok(&t(&a, &a, Mode::Move, &["a.txt"], "/src/A.txt").run(&fb, &Cancel::new()));
    assert_eq!(a.paths(), ["/src", "/src/A.txt"]);
    assert_eq!(a.read("/src/a.txt").unwrap(), b"a");
}

/// A folder that can't be read is a question like any failed file, not the end of the whole operation.
#[test]
fn an_unreadable_folder_is_asked_about() {
    let (a, b) = (Arc::new(MemFs::new("a")), Arc::new(MemFs::new("b")));
    tree(&a);
    b.add_dir("/dst");
    a.fail_on("/src/dir/sub");
    let fb = Script::answers(&[Answer::Skip]);
    let r = t(&a, &b, Mode::Copy, &["dir", "a.txt"], "/dst").run(&fb, &Cancel::new());
    assert_eq!((r.done, r.failed), (2, 1), "{r:?}");
    assert!(b.exists("/dst/a.txt") && b.exists("/dst/dir/b.txt") && !b.exists("/dst/dir/sub/c.txt"));
    // Delete: the same.
    let fb = Script::answers(&[Answer::Skip]);
    let r = delete(&*a, &VPath::new("/src"), &["dir".into()], &fb, &Cancel::new());
    assert_eq!(r.failed, 1, "{r:?}");
    assert!(a.exists("/src/dir/sub/c.txt") && !a.exists("/src/dir/b.txt"));
}

/// A new name with '/' or `..` from the conflict dialog is not a name: asked again.
#[test]
fn a_bad_new_name_is_asked_again() {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/src/f", "new").add_file("/dst/f", "old");
    let before = a.paths();
    for bad in ["x/y", "..", ""] {
        let fb = Script::answers(&[Answer::Rename(bad.into()), Answer::Skip]);
        let r = t(&a, &a, Mode::Copy, &["f"], "/dst").run(&fb, &Cancel::new());
        assert_eq!((r.skipped, fb.asked().len()), (1, 2), "{bad:?}: {r:?}");
        assert_eq!(a.paths(), before);
    }
}

/// `rename` declared, but not for this pair (the local disk between volumes): file by file.
#[test]
fn a_move_falls_back_when_rename_is_not_possible_here() {
    let a = Arc::new(MemFs::new("a").rename_unsupported());
    tree(&a);
    let r = t(&a, &a, Mode::Move, &["dir", "a.txt"], "/dst").run(&Script::default(), &Cancel::new());
    ok(&r);
    assert_eq!(r.done, 3, "{r:?}");
    assert_eq!(a.paths(), ["/dst", "/dst/a.txt", "/dst/dir", "/dst/dir/b.txt", "/dst/dir/sub", "/dst/dir/sub/c.txt", "/src", "/src/empty"]);
}

/// FAT, SMB and S3 keep times to 1–2 s: a copy made from a file is not "older" than it.
#[test]
fn overwrite_older_allows_for_rounded_times() {
    let (a, b) = (Arc::new(MemFs::new("a")), Arc::new(MemFs::new("b")));
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    a.add_file("/src/same", "new").add_file("/src/newer", "new");
    a.set_modified("/src/same", t0 + Duration::from_millis(1500));
    a.set_modified("/src/newer", t0 + Duration::from_secs(10));
    b.add_file("/dst/same", "old").add_file("/dst/newer", "old");
    b.set_modified("/dst/same", t0);
    b.set_modified("/dst/newer", t0);
    let fb = Script::answers(&[Answer::OverwriteOlder]);
    let r = t(&a, &b, Mode::Copy, &["newer", "same"], "/dst").run(&fb, &Cancel::new());
    assert_eq!((r.done, r.skipped), (1, 1), "{r:?}");
    assert_eq!(b.read("/dst/same").unwrap(), b"old");
}

/// Where names differ by case, `A` next to `a` is another file: moving `a` onto it is a conflict, not a
/// case-only rename (that once went round in circles).
#[test]
fn another_case_is_another_file_where_case_matters() {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/src/a", "lower").add_file("/src/A", "upper");
    let fb = Script::answers(&[Answer::Skip]);
    let r = t(&a, &a, Mode::Move, &["a"], "/src/A").run(&fb, &Cancel::new());
    assert_eq!((r.skipped, fb.asked().len()), (1, 1), "{r:?}");
    assert_eq!((a.read("/src/a").unwrap(), a.read("/src/A").unwrap()), (b"lower".to_vec(), b"upper".to_vec()));
}

/// Links in memory: the rules for links, without a disk.
fn linked() -> Arc<MemFs> {
    let a = Arc::new(MemFs::new("a"));
    a.add_file("/real/x.txt", "x").add_file("/real/sub/y.txt", "yy").add_file("/src/file.txt", "f");
    a.add_link("/src/tree/to_real", "/real").add_file("/src/tree/plain.txt", "p").add_link("/src/tree/to_file", "/src/file.txt");
    a.add_link("/src/dirlink", "/real");
    a.add_dir("/dst");
    a
}

#[test]
fn links_inside_a_copied_tree() {
    let (a, b) = (linked(), Arc::new(MemFs::new("b")));
    b.add_dir("/dst");
    let r = t(&a, &b, Mode::Copy, &["tree"], "/dst").run(&Script::default(), &Cancel::new());
    // A link to a folder inside the tree is not followed (said so); a link to a file is copied as the file.
    assert_eq!(r.skipped, 1, "{r:?}");
    assert!(r.errors.iter().any(|e| e.contains("to_real") && e.contains("link to a folder")), "{r:?}");
    assert_eq!(b.read("/dst/tree/to_file").unwrap(), b"f");
    assert!(!b.exists("/dst/tree/to_real"));
}

#[test]
fn a_selected_link_to_a_folder_is_followed() {
    let (a, b) = (linked(), Arc::new(MemFs::new("b")));
    b.add_dir("/dst");
    ok(&t(&a, &b, Mode::Copy, &["dirlink"], "/dst").run(&Script::default(), &Cancel::new()));
    assert_eq!(b.read("/dst/dirlink/sub/y.txt").unwrap(), b"yy");
}

#[test]
fn moving_links() {
    // Inside the source: the link itself moves (a rename), the folder stays.
    let a = linked();
    ok(&t(&a, &a, Mode::Move, &["dirlink"], "/dst").run(&Script::default(), &Cancel::new()));
    assert!(matches!(a.stat(&VPath::new("/dst/dirlink")).unwrap().kind, vfs::Kind::Symlink { to_dir: true, .. }));
    assert!(a.exists("/real/sub/y.txt") && !a.exists("/src/dirlink"));
    // To another source: the folder's files are copied, only the link goes.
    let (a, b) = (linked(), Arc::new(MemFs::new("b")));
    b.add_dir("/dst");
    ok(&t(&a, &b, Mode::Move, &["dirlink"], "/dst").run(&Script::default(), &Cancel::new()));
    assert_eq!(b.read("/dst/dirlink/x.txt").unwrap(), b"x");
    assert!(a.exists("/real/x.txt") && a.exists("/real/sub/y.txt"), "the folder behind the link is whole");
    assert!(!a.exists("/src/dirlink"));
}

#[test]
fn deleting_a_link_keeps_its_target() {
    let a = linked();
    let r = delete(&*a, &VPath::new("/src"), &["dirlink".into(), "tree".into()], &Script::default(), &Cancel::new());
    ok(&r);
    assert!(a.exists("/real/x.txt") && a.exists("/real/sub/y.txt") && a.exists("/src/file.txt"));
    assert!(!a.exists("/src/dirlink") && !a.exists("/src/tree"));
    // The size of a folder doesn't count what its links lead to.
    let a = linked();
    let s = dir_size(&*a, &VPath::new("/src/tree"), &Cancel::new()).unwrap();
    assert_eq!((s.files, s.dirs), (3, 0), "{s:?}");
}
