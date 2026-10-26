//! Owned agent subprocess: process-group spawn, group termination, reaping,
//! and the shared stderr logger used by the sdk, kiro and codex runtimes.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::common::shim_eprintln;
use super::pty_log::PtyLogWriter;

/// Poll interval while waiting for a child to exit.
const EXIT_POLL: Duration = Duration::from_millis(50);

/// A child process that leads its own process group, so signals reach every
/// descendant (e.g. the agent behind `bash -lc`), not just the shell.
pub struct OwnedChild {
    child: Child,
}

/// Spawn `bash -lc <cmd>` in `cwd` as a process-group leader with piped stdio.
/// `CLAUDECODE` is removed from the environment to prevent nested detection.
pub fn spawn_piped_agent(
    cmd: &str,
    cwd: &Path,
    envs: &[(&str, &str)],
) -> std::io::Result<OwnedChild> {
    let mut command = Command::new("bash");
    command
        .args(["-lc", cmd])
        .current_dir(cwd)
        .envs(envs.iter().copied());
    OwnedChild::spawn(&mut command)
}

impl OwnedChild {
    /// Spawn `command` with piped stdio, its own process group, and
    /// `CLAUDECODE` removed from its environment.
    pub fn spawn(command: &mut Command) -> std::io::Result<Self> {
        use std::os::unix::process::CommandExt;
        let child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_remove("CLAUDECODE")
            .process_group(0)
            .spawn()?;
        Ok(Self { child })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.child.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Send `sig` to the whole process group. Errors (e.g. ESRCH once every
    /// member is gone) are ignored.
    pub fn signal_group(&self, sig: i32) {
        // The group id equals the leader pid (`process_group(0)`).
        unsafe {
            libc::kill(-(self.child.id() as i32), sig);
        }
    }

    /// SIGKILL the group, then reap the leader so no zombie remains.
    pub fn kill_and_reap(&mut self) {
        self.signal_group(libc::SIGKILL);
        let _ = self.child.wait();
    }

    /// SIGTERM the group, wait up to `grace` for the leader to exit, then
    /// SIGKILL the whole group (catching descendants that ignored SIGTERM or
    /// outlived the shell) and reap. No-op if the child was already reaped.
    pub fn terminate(&mut self, grace: Duration) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        self.signal_group(libc::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(EXIT_POLL);
        }
        self.kill_and_reap();
    }
}

/// Like [`OwnedChild::terminate`] for a child shared behind a mutex, without
/// holding the lock while waiting out the grace period: other threads can
/// still `try_wait` / inspect the child.
pub fn terminate_shared(child: &Mutex<OwnedChild>, grace: Duration) {
    {
        let mut guard = child.lock().unwrap();
        if guard.try_wait().ok().flatten().is_some() {
            return;
        }
        guard.signal_group(libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.lock().unwrap().try_wait().ok().flatten().is_some() {
            break;
        }
        thread::sleep(EXIT_POLL);
    }
    child.lock().unwrap().kill_and_reap();
}

/// Log each stderr line of the child as `[shim-<runtime> <id>] stderr: ...`,
/// mirror it to the PTY log, and pass it to `on_line`.
pub fn spawn_stderr_logger(
    stderr: ChildStderr,
    runtime: &'static str,
    shim_id: String,
    pty_log: Option<Arc<Mutex<PtyLogWriter>>>,
    mut on_line: impl FnMut(&str) + Send + 'static,
) -> JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            shim_eprintln!("[shim-{runtime} {shim_id}] stderr: {line}");
            on_line(&line);
            if let Some(log) = &pty_log {
                let _ = log
                    .lock()
                    .unwrap()
                    .write(format!("[stderr] {line}\n").as_bytes());
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(s) = std::fs::read_to_string(path) {
                if s.ends_with('\n') {
                    return s;
                }
            }
            assert!(Instant::now() < deadline, "file never written");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn terminate_kills_grandchildren_and_reaps() {
        let tmp = tempfile::tempdir().unwrap();
        let pidfile = tmp.path().join("pid");
        let cmd = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let mut child = spawn_piped_agent(&cmd, tmp.path(), &[]).unwrap();
        let grandchild: i32 = wait_for_file(&pidfile).trim().parse().unwrap();
        assert!(pid_alive(grandchild));

        child.terminate(Duration::from_secs(2));

        // Reaped: a second wait yields the cached status.
        assert!(child.try_wait().unwrap().is_some());
        let deadline = Instant::now() + Duration::from_secs(3);
        while pid_alive(grandchild) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!pid_alive(grandchild), "grandchild leaked");
    }

    #[test]
    fn terminate_escalates_to_sigkill_for_term_ignoring_group() {
        let tmp = tempfile::tempdir().unwrap();
        let ready = tmp.path().join("ready");
        let cmd = format!(
            "trap '' TERM; echo ok > {}; while true; do sleep 1; done",
            ready.display()
        );
        let mut child = spawn_piped_agent(&cmd, tmp.path(), &[]).unwrap();
        wait_for_file(&ready);
        let started = Instant::now();
        child.terminate(Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn terminate_shared_does_not_hold_lock_during_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let ready = tmp.path().join("ready");
        let cmd = format!(
            "trap '' TERM; echo ok > {}; while true; do sleep 1; done",
            ready.display()
        );
        let child = Arc::new(Mutex::new(
            spawn_piped_agent(&cmd, tmp.path(), &[]).unwrap(),
        ));
        wait_for_file(&ready);
        let c2 = Arc::clone(&child);
        let t = thread::spawn(move || terminate_shared(&c2, Duration::from_millis(1500)));
        thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        let _ = child.lock().unwrap().try_wait();
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "lock held across grace period"
        );
        t.join().unwrap();
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }

    #[test]
    fn spawn_passes_envs_and_removes_claudecode() {
        use std::io::Read;
        let tmp = tempfile::tempdir().unwrap();
        let mut child = spawn_piped_agent(
            "printf '%s|%s' \"$BATTY_T\" \"${CLAUDECODE-unset}\"",
            tmp.path(),
            &[("BATTY_T", "v")],
        )
        .unwrap();
        let mut out = String::new();
        child
            .take_stdout()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        child.terminate(Duration::from_secs(1));
        assert_eq!(out, "v|unset");
    }
}
