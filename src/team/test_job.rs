//! Background test-command jobs for the daemon poll loop.
//!
//! Verification and smoke commands can run for minutes. Running them inline
//! stalls every other daemon duty (message routing, shim ping/pong, hot
//! reload), so the daemon runs them on a worker thread and checks for the
//! result on later polls.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

/// Process-group id of the test command a job is currently running (0 when
/// no command is running). The worker spawns the command in its own process
/// group so the daemon can kill the whole tree on shutdown or hot reload.
pub(crate) type ChildPgid = Arc<AtomicU32>;

/// Slot value meaning the job was killed. A command spawned after the kill
/// sees it and is killed at once instead of running as an orphan.
const CANCELLED: u32 = u32::MAX;

/// Default wall-clock limit for a test job, in seconds (0 disables the
/// limit). The daemon mirrors `workflow_policy.verification.timeout_secs`
/// here so every spawn site, including the merge queue, is bounded.
static DEFAULT_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(3600);

pub(crate) fn set_default_timeout_secs(secs: u64) {
    DEFAULT_TIMEOUT_SECS.store(secs, Ordering::Relaxed);
}

fn default_timeout() -> Option<Duration> {
    match DEFAULT_TIMEOUT_SECS.load(Ordering::Relaxed) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// Record a freshly spawned child's process group. Returns `false` (and
/// kills the group) if the job was already cancelled.
pub(crate) fn register_child(slot: &AtomicU32, pid: u32) -> bool {
    if slot
        .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return true;
    }
    kill_group(pid);
    false
}

/// Clear the slot after the child exits, keeping a cancellation mark.
pub(crate) fn clear_child(slot: &AtomicU32, pid: u32) {
    let _ = slot.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst);
}

pub(crate) fn is_cancelled(slot: &AtomicU32) -> bool {
    slot.load(Ordering::SeqCst) == CANCELLED
}

fn kill_group(pgid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pgid as libc::pid_t), libc::SIGKILL);
    }
}

/// Error reported by [`TestJob::try_take`] when the wall-clock limit passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TestTimedOut {
    pub(crate) secs: u64,
}

impl std::fmt::Display for TestTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "timed out after {}s", self.secs)
    }
}

impl std::error::Error for TestTimedOut {}

/// A running job as persisted for `batty status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RunningTestInfo {
    pub(crate) label: String,
    pub(crate) started_unix_secs: u64,
}

impl RunningTestInfo {
    /// Human-readable summary such as `verify-eng-1 (running 2m05s)`.
    pub(crate) fn summary(&self, now_unix_secs: u64) -> String {
        let secs = now_unix_secs.saturating_sub(self.started_unix_secs);
        format!("{} (running {}m{:02}s)", self.label, secs / 60, secs % 60)
    }
}

pub(crate) struct TestJob<T> {
    label: String,
    rx: Receiver<Result<T>>,
    child_pgid: ChildPgid,
    started_at: Instant,
    started_unix_secs: u64,
    /// Wall-clock limit measured from spawn; `None` means unbounded.
    timeout: Option<Duration>,
}

impl<T: Send + 'static> TestJob<T> {
    /// Run `work` on a background thread. `work` receives the pgid slot to
    /// hand to the tracked test runner.
    pub(crate) fn spawn(
        label: impl Into<String>,
        work: impl FnOnce(&ChildPgid) -> Result<T> + Send + 'static,
    ) -> Self {
        Self::spawn_with_timeout(label, default_timeout(), work)
    }

    /// Like [`spawn`](Self::spawn) with an explicit wall-clock limit. Once it
    /// is exceeded, `try_take` kills the command's process group and reports
    /// a "timed out" error.
    pub(crate) fn spawn_with_timeout(
        label: impl Into<String>,
        timeout: Option<Duration>,
        work: impl FnOnce(&ChildPgid) -> Result<T> + Send + 'static,
    ) -> Self {
        let label = label.into();
        let (tx, rx) = mpsc::channel();
        let child_pgid: ChildPgid = Arc::new(AtomicU32::new(0));
        let worker_pgid = Arc::clone(&child_pgid);
        let thread_name = format!("batty-test-{label}");
        let spawn_result = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let _ = tx.send(work(&worker_pgid));
            });
        if let Err(error) = spawn_result {
            // The closure (and its sender) was dropped, so `try_take` reports
            // the failure on the next poll instead of hanging forever.
            tracing::warn!(label = %label, error = %error, "failed to spawn test job thread");
        }
        Self {
            label,
            rx,
            child_pgid,
            started_at: Instant::now(),
            started_unix_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            timeout,
        }
    }
}

impl<T> TestJob<T> {
    /// Returns the job result once the worker has finished, `None` while it
    /// is still running.
    pub(crate) fn try_take(&self) -> Option<Result<T>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => {
                let limit = self
                    .timeout
                    .filter(|limit| self.started_at.elapsed() >= *limit)?;
                tracing::warn!(
                    label = %self.label,
                    timeout_secs = limit.as_secs(),
                    "background test job timed out"
                );
                self.kill();
                Some(Err(anyhow::Error::new(TestTimedOut {
                    secs: limit.as_secs(),
                })))
            }
            Err(TryRecvError::Disconnected) => Some(Err(anyhow!(
                "background test job `{}` exited without a result",
                self.label
            ))),
        }
    }

    pub(crate) fn info(&self) -> RunningTestInfo {
        RunningTestInfo {
            label: self.label.clone(),
            started_unix_secs: self.started_unix_secs,
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Kill the running test command's process group and mark the job
    /// cancelled, so a command the worker has not spawned yet is killed as
    /// soon as it starts. The worker then finishes on its own.
    pub(crate) fn kill(&self) {
        let pgid = self.child_pgid.swap(CANCELLED, Ordering::SeqCst);
        if pgid == 0 || pgid == CANCELLED {
            return;
        }
        tracing::info!(label = %self.label, pgid, "killing background test command");
        kill_group(pgid);
    }

    /// Block until the job finishes. Test-only: production code polls.
    #[cfg(test)]
    pub(crate) fn wait(&self, timeout: Duration) -> Option<Result<T>> {
        match self.rx.recv_timeout(timeout) {
            Ok(result) => Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(anyhow!(
                "background test job `{}` exited without a result",
                self.label
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_take_is_none_while_running_then_returns_result() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let job = TestJob::spawn("unit", move |_| {
            release_rx.recv().unwrap();
            Ok(7)
        });
        assert!(job.try_take().is_none());
        release_tx.send(()).unwrap();
        assert_eq!(job.wait(Duration::from_secs(5)).unwrap().unwrap(), 7);
    }

    #[test]
    fn panicking_worker_reports_error_instead_of_hanging() {
        let job: TestJob<u32> = TestJob::spawn("panics", |_| panic!("boom"));
        let result = job.wait(Duration::from_secs(5)).expect("job should finish");
        assert!(result.unwrap_err().to_string().contains("without a result"));
    }

    #[cfg(unix)]
    #[test]
    fn kill_terminates_tracked_command_process_group() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let job = TestJob::spawn("kill", move |pgid| {
            crate::team::task_loop::run_tests_in_worktree_tracked(
                &dir,
                Some("sleep 30; exit 0"),
                Some(pgid),
            )
        });
        assert!(
            crate::team::test_support::wait_until(Duration::from_secs(60), || {
                job.child_pgid.load(Ordering::SeqCst) != 0
            }),
            "command never started"
        );
        let started = Instant::now();
        job.kill();
        // The command sleeps 30s; finishing well inside that proves the kill
        // took effect, while the wide margin tolerates a loaded machine.
        let run = job
            .wait(Duration::from_secs(60))
            .expect("killed job should finish")
            .unwrap();
        assert!(!run.passed);
        assert!(started.elapsed() < Duration::from_secs(25));
    }

    #[cfg(unix)]
    #[test]
    fn job_past_its_timeout_is_killed_and_reported_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let job =
            TestJob::spawn_with_timeout("timeout", Some(Duration::from_secs(1)), move |pgid| {
                crate::team::task_loop::run_tests_in_worktree_tracked(
                    &dir,
                    Some("sleep 30; exit 0"),
                    Some(pgid),
                )
            });
        let mut outcome = None;
        assert!(
            crate::team::test_support::wait_until(Duration::from_secs(25), || {
                outcome = job.try_take();
                outcome.is_some()
            }),
            "timed-out job was never reported"
        );
        let error = outcome.unwrap().unwrap_err().to_string();
        assert_eq!(error, "timed out after 1s");
        // The process group was killed: the worker finishes well before 30s.
        assert!(job.wait(Duration::from_secs(20)).is_some());
    }

    #[test]
    fn running_test_summary_shows_elapsed_minutes_and_seconds() {
        let info = RunningTestInfo {
            label: "verify-eng-1".into(),
            started_unix_secs: 1_000,
        };
        assert_eq!(info.summary(1_125), "verify-eng-1 (running 2m05s)");
        assert_eq!(info.summary(500), "verify-eng-1 (running 0m00s)");
    }

    #[test]
    fn finished_result_wins_over_an_expired_timeout() {
        let job = TestJob::spawn_with_timeout("done", Some(Duration::ZERO), |_| Ok(5));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(job.try_take().unwrap().unwrap(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn kill_before_command_spawns_prevents_it_running() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let marker = dir.join("ran");
        let command = format!("touch '{}'", marker.display());
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let job = TestJob::spawn("early-kill", move |pgid| {
            release_rx.recv().unwrap();
            crate::team::task_loop::run_tests_in_worktree_tracked(&dir, Some(&command), Some(pgid))
        });

        job.kill();
        release_tx.send(()).unwrap();
        let result = job
            .wait(Duration::from_secs(10))
            .expect("job should finish");
        assert!(result.is_err(), "cancelled job must not report a test run");
        assert!(!marker.exists(), "command must not run after kill");
    }

    #[test]
    fn register_child_after_cancel_is_refused() {
        let slot = AtomicU32::new(0);
        slot.store(CANCELLED, Ordering::SeqCst);
        // pid 0 would signal our own group; use an impossible pid instead.
        assert!(!register_child(&slot, i32::MAX as u32));
        assert!(is_cancelled(&slot));
        clear_child(&slot, i32::MAX as u32);
        assert!(is_cancelled(&slot), "clearing must keep the cancel mark");
    }
}
