//! uison-commander: a two-panel file manager in the spirit of Norton and Total Commander.
//!
//! `uison-commander [folder] [--mem] [--slow ms] [--png out/]`
//! - `--mem` adds a source in memory with sample files (always there in debug builds);
//! - `--slow ms` makes that source slow, to watch the UI stay responsive;
//! - `--png out/` renders the screen without a window into PNG files (for checks).

#[macro_use]
mod i18n;

mod app;
mod connect;
mod dialog;
mod format;
mod icons;
mod jobs;
mod keymap;
#[cfg(target_os = "macos")]
mod macos;
mod panel;
mod screen;
mod secrets;
mod state;
mod viewer;
#[cfg(windows)]
mod windows;

#[cfg(any(target_os = "macos", windows))]
mod check;

use std::sync::Arc;

use vfs::{MemFs, Source};
use vfs_local::LocalFs;

pub use vfs_local::os_path;

/// Help → About links here (`homepage` of Cargo.toml).
pub const HOMEPAGE: &str = env!("CARGO_PKG_HOMEPAGE");

/// Sample files in memory: folders, text, a large file for the progress dialog, an extra action.
pub fn demo_source(slow_ms: u64) -> MemFs {
    let fs = MemFs::new("demo").slow(slow_ms);
    fs.add_file("/readme.txt", "A source in memory.\nCopy files between it and the disk: the same code as local to local.\n");
    fs.add_file("/docs/notes.md", "# Notes\n\n- one\n- two\n");
    fs.add_file("/docs/report.pdf", vec![0x25u8; 180_000]);
    fs.add_file("/docs/archive/2025.csv", "a,b,c\n1,2,3\n");
    fs.add_file("/photos/cat.jpg", vec![0xFFu8; 2_100_000]);
    fs.add_file("/big/video.bin", vec![7u8; 64 << 20]);
    let log: String = (1..=20_000).map(|i| format!("{i:6} line of a long log with some text to search for, row {i}\n")).collect();
    fs.add_file("/big/server.log", log);
    fs.add_file("/.hidden", "hidden file\n");
    fs.add_file("/a very long file name that does not fit into the name column of a panel at all.txt", "");
    fs.add_file("/backup.json.1770532892562", "");
    fs.add_dir("/empty");
    fs
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let slow = value("--slow").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut sources: Vec<Arc<dyn Source>> = vec![Arc::new(LocalFs::new())];
    if flag("--mem") || cfg!(debug_assertions) || slow > 0 {
        sources.push(Arc::new(demo_source(slow)));
    }
    // The first argument that is neither a flag nor a flag's value: the folder for the left panel.
    let with_value = ["--slow", "--png", "--bench"];
    let is_value = |i: usize| i > 0 && with_value.contains(&args[i - 1].as_str());
    let folder = (0..args.len()).find(|&i| !args[i].starts_with("--") && !is_value(i)).and_then(|i| vfs_local::resolve(&args[i]));
    #[cfg(any(target_os = "macos", windows))]
    if let Some(dir) = value("--png") {
        return check::png(&dir);
    }
    #[cfg(any(target_os = "macos", windows))]
    if let Some(dir) = value("--bench").and_then(|d| vfs_local::resolve(&d)) {
        return check::bench(dir);
    }
    let ui = app::build(sources, app::Start::Saved(folder));
    uison::run(ui, "uison-commander");
}
