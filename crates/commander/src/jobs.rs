//! Worker threads and the way back to the UI.
//!
//! A worker never touches the UI: it puts a message into the mailbox and wakes the UI thread, where the
//! `pump` action takes the messages out with access to the app state.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use ops::{Answer, Feedback, Progress, Question, Report, Size};
use uison::ui::Handle;
use vfs::{ActionInfo, ActionResult, Entry, Root, Source, VPath};

pub enum Msg {
    Listed {
        side: usize,
        seq: u64,
        /// The source that was read: the panel switches to it together with the folder.
        src: Arc<dyn Source>,
        dir: VPath,
        res: vfs::Result<Vec<Entry>>,
        /// Free and total bytes of the volume, if the source knows.
        space: Option<(u64, u64)>,
        focus: Option<String>,
    },
    /// The read of `seq` is still going after a while.
    Slow {
        side: usize,
        seq: u64,
    },
    Size {
        side: usize,
        /// The source and folder it was counted in: the panel may have moved on.
        src: String,
        dir: VPath,
        name: String,
        res: vfs::Result<Size>,
    },
    /// The roots of source `index`, for the Alt+F1 list `seq`.
    Roots {
        seq: u64,
        index: usize,
        res: vfs::Result<Vec<Root>>,
    },
    Progress {
        job: u64,
        p: Progress,
    },
    Ask {
        job: u64,
        q: Question,
    },
    Done {
        job: u64,
        report: Report,
    },
    /// Quitting waited long enough for the cancelled operation.
    Quit,
    /// The notice `n` has been up long enough.
    ToastOff {
        n: u64,
    },
    Mkdir {
        side: usize,
        res: vfs::Result<VPath>,
    },
    /// Shift+F4: the file made (then it opens in the editor).
    NewFile {
        side: usize,
        res: vfs::Result<VPath>,
    },
    Action {
        res: vfs::Result<ActionResult>,
    },
    /// A source action with its parameters filled in (`prepare_action`), ready to be asked.
    ActionReady {
        side: usize,
        targets: Vec<VPath>,
        res: vfs::Result<ActionInfo>,
    },
    /// A new storage answered (its roots were listed) — or why not.
    Connected {
        side: usize,
        /// The attempt (a cancelled one is ignored when it answers).
        seq: u64,
        res: vfs::Result<Arc<dyn Source>>,
        /// For the user: a new SSH host was remembered.
        note: Option<String>,
        /// What to remember of it.
        profile: Option<crate::connect::Profile>,
        /// The folder to open (a panel restored at startup); otherwise the source's home.
        dir: Option<VPath>,
        /// Connected by itself (at startup): no "Connecting…" to close, errors only as a note.
        quiet: bool,
    },
    /// The connection needs a yes first (a server's key never seen): ask, and connect again with
    /// `(key, value)` added if the user agrees.
    Confirm {
        side: usize,
        seq: u64,
        index: usize,
        args: vfs::ActionArgs,
        title: String,
        text: String,
        button: String,
        key: String,
        value: String,
        dir: Option<VPath>,
        quiet: bool,
    },
    /// A remembered connection's secrets from the Keychain (`None`: not all there — ask them).
    Secrets {
        side: usize,
        profile: usize,
        args: Option<vfs::ActionArgs>,
        dir: Option<VPath>,
        quiet: bool,
    },
    /// Properties applied: the entry's name now.
    Changed {
        side: usize,
        res: vfs::Result<String>,
    },
    View {
        seq: u64,
        offset: u64,
        res: vfs::Result<Vec<u8>>,
    },
    Image {
        seq: u64,
        res: vfs::Result<Vec<u8>>,
    },
    Found {
        seq: u64,
        res: vfs::Result<Option<u64>>,
    },
}

pub struct Mailbox {
    q: Mutex<Vec<Msg>>,
    handle: Handle,
}

impl Mailbox {
    pub fn new(handle: Handle) -> Arc<Mailbox> {
        Arc::new(Mailbox { q: Mutex::new(Vec::new()), handle })
    }

    pub fn send(&self, m: Msg) {
        let mut q = self.q.lock().unwrap();
        let wake = q.is_empty();
        q.push(m);
        drop(q);
        // One wake-up per batch: a fast copy sends progress far more often than frames are drawn.
        if wake {
            self.handle.post(|cx| cx.timer(0, "pump"));
        }
    }

    pub fn take(&self) -> Vec<Msg> {
        std::mem::take(&mut *self.q.lock().unwrap())
    }
}

/// Run `f` in a worker thread.
pub fn spawn(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new().name("worker".into()).spawn(f).expect("spawn a worker");
}

/// An operation's feedback: progress goes to the mailbox, a question blocks until the UI answers.
pub struct JobFeedback {
    pub job: u64,
    pub mb: Arc<Mailbox>,
    answers: Mutex<Receiver<Answer>>,
}

impl JobFeedback {
    pub fn new(job: u64, mb: Arc<Mailbox>) -> (JobFeedback, Sender<Answer>) {
        let (tx, rx) = channel();
        (JobFeedback { job, mb, answers: Mutex::new(rx) }, tx)
    }
}

impl Feedback for JobFeedback {
    fn progress(&self, p: &Progress) {
        self.mb.send(Msg::Progress { job: self.job, p: p.clone() });
    }

    fn ask(&self, q: Question) -> Answer {
        self.mb.send(Msg::Ask { job: self.job, q });
        // The UI is gone or dropped the sender: stop.
        self.answers.lock().unwrap().recv().unwrap_or(Answer::Cancel)
    }
}
