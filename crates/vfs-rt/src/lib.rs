//! The network sources (S3, SFTP) are async; the contract is not — it is called from worker threads. They
//! share this one Tokio runtime (two threads) and wait on it with `run`, or with `run_cancel` where the
//! caller can stop: a request in flight is dropped at once instead of waited for to its timeout.

use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::runtime::Runtime;
use vfs::{Cancel, Error, Result};

pub fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("vfs-net").enable_all().build().expect("tokio runtime")
    })
}

/// Wait for `f` on the runtime (from a worker thread, never from inside the runtime).
pub fn run<F: Future>(f: F) -> F::Output {
    rt().block_on(f)
}

/// How often a wait looks at its `Cancel`.
const POLL: Duration = Duration::from_millis(50);

/// `run`, but `Cancelled` as soon as `cancel` is set: `f` is dropped where it is (an HTTP request, an SFTP
/// request waiting for its answer).
pub fn run_cancel<F: Future>(f: F, cancel: &Cancel) -> Result<F::Output> {
    let c = cancel.clone();
    rt().block_on(async move {
        tokio::select! {
            out = f => Ok(out),
            () = async {
                while !c.is_cancelled() {
                    tokio::time::sleep(POLL).await;
                }
            } => Err(Error::Cancelled),
        }
    })
}

/// `run_cancel` on the worker thread's current cancel (`Cancel::scoped`).
pub fn run_current<F: Future>(f: F) -> Result<F::Output> {
    run_cancel(f, &Cancel::current())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_wait_is_cancelled_quickly() {
        let c = Cancel::new();
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.cancel();
        });
        let t = std::time::Instant::now();
        let r = run_cancel(async { tokio::time::sleep(Duration::from_secs(30)).await }, &c);
        assert_eq!(r, Err(Error::Cancelled));
        assert!(t.elapsed() < Duration::from_millis(600), "{:?}", t.elapsed());
        assert_eq!(run_cancel(async { 7 }, &Cancel::new()), Ok(7));
    }
}
