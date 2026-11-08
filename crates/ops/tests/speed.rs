//! Copy speed against `cp` (ignored: writes 1 GB). `cargo test --release -p ops --test speed -- --ignored --nocapture`

use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use ops::{Answer, Feedback, Mode, Progress, Question, Transfer};
use vfs::*;
use vfs_local::LocalFs;

struct Quiet;

impl Feedback for Quiet {
    fn progress(&self, _: &Progress) {}
    fn ask(&self, _: Question) -> Answer {
        Answer::Overwrite
    }
}

/// The local disk under another id: a copy has to stream (no clone), as between two volumes.
struct Other(LocalFs);

impl Source for Other {
    fn info(&self) -> SourceInfo {
        SourceInfo { id: "other".into(), ..self.0.info() }
    }
    fn roots(&self, c: &Cancel) -> Result<Vec<Root>> {
        self.0.roots(c)
    }
    fn home(&self) -> VPath {
        self.0.home()
    }
    fn list(&self, d: &VPath, c: &Cancel) -> Result<Vec<Entry>> {
        self.0.list(d, c)
    }
    fn stat(&self, p: &VPath) -> Result<Entry> {
        self.0.stat(p)
    }
    fn open_read(&self, p: &VPath, o: u64) -> Result<Box<dyn Read + Send>> {
        self.0.open_read(p, o)
    }
    fn create(&self, p: &VPath, o: &CreateOpts) -> Result<Box<dyn Writer>> {
        self.0.create(p, o)
    }
    fn mkdir(&self, p: &VPath) -> Result<()> {
        self.0.mkdir(p)
    }
    fn remove(&self, p: &VPath) -> Result<()> {
        self.0.remove(p)
    }
}

#[test]
#[ignore]
fn copy_one_gigabyte() {
    let dir = std::env::temp_dir().join("uc-speed");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::create_dir_all(dir.join("c")).unwrap();
    let data: Vec<u8> = (0..(1u32 << 30)).map(|i| (i * 2654435761) as u8).collect();
    std::fs::write(dir.join("a/big"), &data).unwrap();
    drop(data);
    let dir = dir.canonicalize().unwrap();
    let base = vfs_local::vpath(&dir);
    let run = |dst: Arc<dyn Source>, to: &str| {
        let t = Transfer {
            mode: Mode::Copy,
            src: Arc::new(LocalFs::new()),
            src_dir: base.child("a"),
            names: vec!["big".into()],
            dst,
            target: base.child(to).to_string(),
        };
        let t0 = Instant::now();
        let r = t.run(&Quiet, &Cancel::new());
        assert!(r.errors.is_empty(), "{r:?}");
        t0.elapsed()
    };
    let clone = run(Arc::new(LocalFs::new()), "b");
    let stream = run(Arc::new(Other(LocalFs::new())), "c");
    let _ = std::fs::remove_file(dir.join("c/big"));
    let t0 = Instant::now();
    let st = std::process::Command::new("cp").arg(dir.join("a/big")).arg(dir.join("c/big")).status().unwrap();
    assert!(st.success());
    let cp = t0.elapsed();
    println!(
        "1 GB: clone {} ms, stream {} ms ({:.0} MB/s), cp {} ms",
        clone.as_millis(),
        stream.as_millis(),
        1024.0 / stream.as_secs_f64(),
        cp.as_millis()
    );
    std::fs::remove_dir_all(dir).unwrap();
}
