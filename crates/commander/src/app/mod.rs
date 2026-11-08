//! The app: two panels, keys, dialogs, operations in worker threads, the viewer.
//!
//! Every handler runs on the UI thread and never calls a source: it starts a worker and gets the result
//! from the mailbox (`pump`).
//!
//! One `impl App` per concern: `panels` (folders, cursor, menu), `keys` (commands), `operations` (copy,
//! move, delete, their questions), `dialogs` (the queue, notices, source list, help), `sources` (properties,
//! source actions, connecting), `view` (F3), `mailbox` (what workers send), `misc` (state, quit, mouse),
//! `settings` (theme, language),
//! `build` (the screen and its handlers).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use ops::{Answer, Mode, Progress, Question, Transfer};
use uison::ui::{Cx, Data, Json, Ui, obj};
use vfs::{ActionResult, Cancel, Entry, Kind as EntryKind, Source, VPath, same_storage};

use crate::dialog::{Act, Button, Dialog, Kind, ListItem, Status};
use crate::jobs::{JobFeedback, Mailbox, Msg, spawn};
use crate::keymap::{Cmd, command};
use crate::panel::{Panel, Sort, SortKey};
use crate::screen::{ROW_H, VIEW_CHROME_H, VIEW_LINE_H};
use crate::viewer::{self, Mode as ViewMode, Viewer};
use crate::{format, state};

mod build;
mod dialogs;
mod keys;
mod mailbox;
mod misc;
mod operations;
mod panels;
mod settings;
mod sources;
mod view;

pub use build::{Start, build};

/// A running operation.
struct Job {
    id: u64,
    title: String,
    cancel: Cancel,
    answer: Sender<Answer>,
    /// Hidden into the strip under the panels.
    background: bool,
    /// A question is open.
    asking: bool,
    last: Progress,
    /// From where to where, for the progress dialog.
    route: String,
    /// (when, bytes done): the last seconds, for the speed now (not the average with the questions in it).
    samples: std::collections::VecDeque<(Instant, u64)>,
    /// After it: the panel and the name to put the cursor on.
    focus: Option<(usize, String)>,
    /// Clear the selection of this panel when done.
    deselect: Option<usize>,
}

pub struct App {
    pub panels: [Panel; 2],
    pub active: usize,
    pub sources: Vec<Arc<dyn Source>>,
    mb: Arc<Mailbox>,
    dlg: Option<Dialog>,
    job: Option<Job>,
    view: Option<Viewer>,
    /// Dialogs that came while a question of the operation was open (results, errors): shown after it.
    queued: Vec<Dialog>,
    /// The open conflict question, to come back to from its "Rename…".
    question: Option<Dialog>,
    /// Quit when the cancelled operation ends.
    quitting: bool,
    /// The notice showing (its number: a later one outlives the timer of an earlier one).
    toast: u64,
    /// Column widths (ext, size, date, attributes) once changed (kept in the state).
    pub cols: [Option<[f64; 4]>; 2],
    /// Remembered connections (`connect::Profile`).
    profiles: Vec<crate::connect::Profile>,
    /// The connection being made (its seq), while "Connecting…" shows.
    connecting: Option<u64>,
    /// A connection waiting for the user's yes: (panel, connector, answers with the yes added, folder).
    confirming: Option<(usize, usize, vfs::ActionArgs, Option<VPath>)>,
    /// The connection form that is open (it stays until the connection works).
    form: Option<Form>,
    /// The Alt+F1/F2 list being filled: its seq, panel, the roots of each source as they come, and how to
    /// stop the reads still going.
    roots: Option<RootsList>,
    seq: u64,
    /// The theme: `system`, `light` or `dark`.
    pub theme: String,
    /// The settings panel is open.
    settings: bool,
    /// The last saved state, to save only changes.
    saved: String,
    persist: bool,
}

type Shared = Rc<RefCell<App>>;

/// The open connection form: the attempt it waits for, and what it was.
#[derive(Default)]
struct Form {
    seq: Option<u64>,
    test: bool,
    /// The remembered connection being changed: replaced by the new one when it works.
    edit: Option<usize>,
    /// A server key the user trusted in this form: (answer, fingerprint), sent with every attempt after.
    trust: Option<(String, String)>,
}

/// The Alt+F1/F2 list being filled.
struct RootsList {
    seq: u64,
    side: usize,
    /// Each source's roots as they come (`None` — still being read).
    results: Vec<Option<vfs::Result<Vec<vfs::Root>>>>,
    cancel: Cancel,
}

impl App {
    pub(super) fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    pub(super) fn source_index(&self, src: &Arc<dyn Source>) -> usize {
        let id = src.info().id;
        self.sources.iter().position(|s| s.info().id == id).unwrap_or(0)
    }
}

/// The columns after the name, and their widths to start with.
const COL_KEYS: [&str; 4] = ["ext", "size", "time", "attr"];
const COLS: [f64; 4] = [52.0, 92.0, 96.0, 74.0];

/// How long a notice stays.
const TOAST_FOR: std::time::Duration = std::time::Duration::from_secs(4);

/// A folder read longer than this shows "Loading…".
const SLOW_LIST: std::time::Duration = std::time::Duration::from_millis(150);

/// How many source actions the menu shows (each needs its own handler).
const ACTION_SLOTS: usize = 8;

/// `left-r12` → (0, 12).
fn row_key(key: &str) -> Option<(usize, usize)> {
    let (side, row) = key.split_once("-r")?;
    let side = match side {
        "left" => 0,
        "right" => 1,
        _ => return None,
    };
    Some((side, row.parse().ok()?))
}

/// Parameters with secrets get "Remember … in the Keychain" (ticked), where there is a Keychain.
fn with_remember(mut ps: Vec<vfs::Param>) -> Vec<vfs::Param> {
    if crate::secrets::available() && ps.iter().any(|p| matches!(p, vfs::Param::Secret { .. })) {
        ps.push(vfs::Param::Flag { id: "remember".into(), title: "Remember the password in the Keychain".into(), default: true });
    }
    ps
}

/// A source's name as a group title: `local` → `This Mac` (the disk), the rest as they are.
fn source_title(t: &str) -> String {
    if t == "local" { if cfg!(target_os = "macos") { t!("This Mac").into() } else { t!("This computer").into() } } else { t.to_string() }
}

/// `"notes.md"`, `the folder "docs"`, `3 objects (1 folder, 2 files)`.
fn what_of(targets: &[Entry]) -> String {
    let dirs = targets.iter().filter(|e| e.is_dir()).count() as u64;
    let files = targets.len() as u64 - dirs;
    match targets {
        [e] if e.is_dir() => t!("the folder \"{}\"", e.name),
        [e] => format!("\"{}\"", e.name),
        _ => {
            let parts: Vec<String> = [(dirs, "folder"), (files, "file")].iter().filter(|(n, _)| *n > 0).map(|(n, w)| format::count(*n, w)).collect();
            format!("{} ({})", format::count(targets.len() as u64, "object"), parts.join(", "))
        }
    }
}

/// `file:///Users/x/My%20file.txt`.
fn file_url(p: &str) -> String {
    let mut s = String::from("file://");
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn on(ui: &mut Ui, app: &Shared, action: &str, f: impl Fn(&mut App, &mut Cx) + 'static) {
    let app = app.clone();
    ui.on(action, move |cx| f(&mut app.borrow_mut(), cx));
}
