//! The single process runner for every `git` invocation Batty makes.
//!
//! All production git calls go through [`cmd`] (or [`cmd_global`] for the
//! rare command that has no repository, such as `git --version`). The runner
//! owns the child from spawn to reap:
//!
//! - the git binary is resolved once per process (see [`program`]);
//! - `-C <repo>` is always passed, stdin is null, and stdout/stderr are drained
//!   concurrently so a chatty command can never block on a full pipe;
//! - inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE` are removed so an
//!   ambient hook environment cannot redirect the command to another repo, and
//!   `GIT_TERMINAL_PROMPT=0` / `GIT_EDITOR=true` keep git from waiting on a
//!   human;
//! - the child runs in its own process group, and on timeout the whole group
//!   is killed and reaped.
//!
//! A nonzero exit is an error unless [`GitCommand::allow_fail`] is set, in
//! which case the caller inspects [`GitOutput::status`].

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// Default timeout for local read/plumbing git commands.
/// Override with `BATTY_GIT_TIMEOUT_SECS`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Default timeout for subcommands that may talk to a remote, rewrite the
/// object store, or run repository hooks (`fetch`, `push`, `gc`, `commit`,
/// `merge`, `rebase`, `checkout`, `worktree`, ...).
/// Override with `BATTY_GIT_LONG_TIMEOUT_SECS`.
pub const LONG_TIMEOUT: Duration = Duration::from_secs(600);

const LONG_RUNNING_SUBCOMMANDS: &[&str] = &[
    // network
    "fetch",
    "push",
    "pull",
    "clone",
    "ls-remote",
    "remote",
    "submodule",
    // object store maintenance
    "gc",
    "repack",
    "prune",
    // may run user hooks or touch the whole checkout
    "commit",
    "merge",
    "rebase",
    "cherry-pick",
    "revert",
    "am",
    "checkout",
    "switch",
    "worktree",
    // walk or rewrite the index/working tree; legitimately slow on big repos
    "reset",
    "stash",
    "add",
    "status",
    "clean",
    "diff",
    "merge-tree",
];

/// Subcommands that may run hooks, signing programs or credential helpers
/// that need the user's terminal when batty itself runs on one.
const INTERACTIVE_SUBCOMMANDS: &[&str] = &["commit", "merge", "rebase", "tag", "push"];

/// How long a timed-out command gets to exit after SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// The git subcommand in `args`, skipping global options such as
/// `-c key=value` and `-C dir`.
fn subcommand_of(args: &[OsString]) -> Option<String> {
    let mut skip_next = false;
    for arg in args {
        let arg = arg.to_string_lossy();
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-c" || arg == "-C" {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.into_owned());
    }
    None
}

/// Whether `subcommand` gets the long timeout class. Read-only plumbing
/// (`rev-parse`, `rev-list`, `for-each-ref`, `config`, ...) stays short.
fn is_long_running(subcommand: Option<&str>) -> bool {
    subcommand.is_some_and(|sub| LONG_RUNNING_SUBCOMMANDS.contains(&sub))
}

/// Whether the child gets its own process group. It does, except when batty
/// runs on a terminal and the command may need that terminal (hooks reading
/// `/dev/tty`, signing prompts): a separate background group would get
/// SIGTTIN on `/dev/tty` reads and would not receive the terminal's Ctrl-C.
/// The daemon has no TTY, so it always gets a group.
fn use_own_process_group(is_tty: bool, subcommand: Option<&str>) -> bool {
    !(is_tty && subcommand.is_some_and(|sub| INTERACTIVE_SUBCOMMANDS.contains(&sub)))
}

fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty has no memory-safety preconditions.
        unsafe { libc::isatty(0) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn timeout_from_env(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// Candidate git binaries, probed in order. The absolute fallbacks cover
/// launchd/daemon environments whose `PATH` lacks the user's git.
const GIT_CANDIDATES: &[&str] = &["git", "/usr/bin/git", "/opt/homebrew/bin/git"];

/// Grace period for collecting pipe output after the child exits or is killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The git executable used for every command, resolved once per process.
pub fn program() -> &'static Path {
    static PROGRAM: OnceLock<PathBuf> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        GIT_CANDIDATES
            .iter()
            .find(|candidate| {
                Command::new(candidate)
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success())
            })
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("git"))
    })
}

/// Start building a git command that runs against `repo` (`git -C <repo>`).
pub fn cmd(repo: impl AsRef<Path>) -> GitCommand {
    GitCommand::new(Some(repo.as_ref().to_path_buf()))
}

/// Start building a git command with no repository (`git --version`,
/// `git clone`, ...). Runs in the current process directory.
pub fn cmd_global() -> GitCommand {
    GitCommand::new(None)
}

/// Captured result of a git command.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn code(&self) -> Option<i32> {
        self.status.code()
    }

    /// Stdout decoded lossily.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr decoded lossily.
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    /// Stdout decoded lossily with surrounding whitespace removed.
    pub fn stdout_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    /// Stderr decoded lossily with surrounding whitespace removed.
    pub fn stderr_trimmed(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

/// Why a git command did not produce a usable result.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// The git process could not be started (binary missing, bad cwd, ...).
    #[error("failed to launch `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    /// git ran and exited unsuccessfully.
    #[error("`{command}` failed ({status}): {}", .stderr.trim())]
    Failed {
        command: String,
        status: ExitStatus,
        stdout: String,
        stderr: String,
    },
    /// git did not finish in time; its process group was killed.
    #[error("`{command}` timed out after {}s", .timeout.as_secs())]
    Timeout {
        command: String,
        timeout: Duration,
        stderr: String,
    },
    /// Waiting on or collecting output from the child failed.
    #[error("failed while waiting for `{command}`: {source}")]
    Wait {
        command: String,
        #[source]
        source: std::io::Error,
    },
}

impl GitError {
    /// Captured stderr, if git produced any before failing.
    pub fn stderr(&self) -> &str {
        match self {
            Self::Failed { stderr, .. } | Self::Timeout { stderr, .. } => stderr,
            Self::Spawn { .. } | Self::Wait { .. } => "",
        }
    }

    /// The most useful one-line explanation: git's trimmed stderr when it
    /// wrote any, otherwise this error's own message.
    pub fn detail(&self) -> String {
        let stderr = self.stderr().trim();
        if stderr.is_empty() {
            self.to_string()
        } else {
            stderr.to_string()
        }
    }

    /// Human-readable command line that failed.
    pub fn command(&self) -> &str {
        match self {
            Self::Spawn { command, .. }
            | Self::Failed { command, .. }
            | Self::Timeout { command, .. }
            | Self::Wait { command, .. } => command,
        }
    }
}

/// Builder for one git invocation.
#[derive(Debug, Clone)]
#[must_use = "a GitCommand does nothing until `.run()` is called"]
pub struct GitCommand {
    repo: Option<PathBuf>,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    timeout: Option<Duration>,
    allow_fail: bool,
    /// Internal: skip stale `index.lock` cleanup after a timeout (set for the
    /// cleanup's own `rev-parse`, which must not recurse).
    skip_lock_cleanup: bool,
}

impl GitCommand {
    fn new(repo: Option<PathBuf>) -> Self {
        Self {
            repo,
            args: Vec::new(),
            envs: Vec::new(),
            timeout: None,
            allow_fail: false,
            skip_lock_cleanup: false,
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| arg.as_ref().to_os_string()));
        self
    }

    /// Set an environment variable for the child. Applied after the runner's
    /// hygiene defaults, so it can deliberately override them (for example a
    /// temporary `GIT_INDEX_FILE`).
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.envs.push((
            key.as_ref().to_os_string(),
            Some(value.as_ref().to_os_string()),
        ));
        self
    }

    /// Remove an environment variable for the child.
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        self.envs.push((key.as_ref().to_os_string(), None));
        self
    }

    /// Override the timeout (default: [`DEFAULT_TIMEOUT`], or
    /// [`LONG_TIMEOUT`] for network/hook-running subcommands).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Return `Ok` for a nonzero exit; the caller inspects `status`.
    /// Spawn failures and timeouts are still errors.
    pub fn allow_fail(mut self) -> Self {
        self.allow_fail = true;
        self
    }

    /// Display form of the command, used in diagnostics.
    pub fn display(&self) -> String {
        let mut parts = vec!["git".to_string()];
        if let Some(repo) = &self.repo {
            parts.push("-C".to_string());
            parts.push(repo.display().to_string());
        }
        parts.extend(
            self.args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        );
        parts.join(" ")
    }

    fn effective_timeout(&self) -> Duration {
        if let Some(timeout) = self.timeout {
            return timeout;
        }
        if is_long_running(subcommand_of(&self.args).as_deref()) {
            timeout_from_env("BATTY_GIT_LONG_TIMEOUT_SECS", LONG_TIMEOUT)
        } else {
            timeout_from_env("BATTY_GIT_TIMEOUT_SECS", DEFAULT_TIMEOUT)
        }
    }

    /// Whether this command runs in its own process group.
    fn own_process_group(&self) -> bool {
        use_own_process_group(stdin_is_tty(), subcommand_of(&self.args).as_deref())
    }

    fn build(&self, grouped: bool) -> Command {
        let mut command = Command::new(program());
        if let Some(repo) = &self.repo {
            command.arg("-C").arg(repo);
        }
        command
            .args(&self.args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.envs {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        #[cfg(unix)]
        if grouped {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(not(unix))]
        let _ = grouped;

        command
    }

    /// Run the command to completion (or timeout) and capture its output.
    pub fn run(self) -> Result<GitOutput, GitError> {
        let display = self.display();
        let timeout = self.effective_timeout();
        let grouped = self.own_process_group();

        let mut command = self.build(grouped);

        let spawned_at = SystemTime::now();
        let mut child = command.spawn().map_err(|source| GitError::Spawn {
            command: display.clone(),
            source,
        })?;
        let pid = child.id();

        let (tx, rx) = mpsc::channel::<(Stream, std::io::Result<Vec<u8>>)>();
        let mut pending = 0;
        if let Some(stdout) = child.stdout.take() {
            spawn_drain(Stream::Stdout, stdout, tx.clone());
            pending += 1;
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_drain(Stream::Stderr, stderr, tx.clone());
            pending += 1;
        }
        drop(tx);

        let deadline = Instant::now() + timeout;
        let status = match wait_until(&mut child, deadline) {
            Ok(Some(status)) => status,
            Ok(None) => {
                terminate_child(&mut child, grouped, KILL_GRACE);
                if !self.skip_lock_cleanup
                    && let Some(repo) = &self.repo
                {
                    remove_stale_index_lock(repo, spawned_at, grouped.then_some(pid));
                }
                let streams = collect(&rx, pending, Instant::now() + DRAIN_GRACE);
                return Err(GitError::Timeout {
                    command: display,
                    timeout,
                    stderr: lossy(&streams.stderr),
                });
            }
            Err(source) => {
                kill_group(&mut child, grouped);
                return Err(GitError::Wait {
                    command: display,
                    source,
                });
            }
        };

        // git exited, so everything it wrote is already in the pipes and the
        // readers finish almost immediately. If they don't, a leftover
        // descendant in git's process group still holds a pipe open: kill the
        // group (it still has a live member, so its id cannot have been
        // reused) and give the readers one more grace period.
        let mut streams = collect(&rx, pending, Instant::now() + DRAIN_GRACE);
        if streams.pending > 0 {
            kill_group_by_pid(child.id(), grouped);
            let late = collect(&rx, streams.pending, Instant::now() + DRAIN_GRACE);
            if late.pending > 0 {
                return Err(GitError::Timeout {
                    command: display,
                    timeout,
                    stderr: lossy(&streams.stderr),
                });
            }
            streams.merge(late);
        }
        if let Some(source) = streams.error {
            return Err(GitError::Wait {
                command: display,
                source,
            });
        }

        let output = GitOutput {
            status,
            stdout: streams.stdout,
            stderr: streams.stderr,
        };
        if !self.allow_fail && !output.status.success() {
            return Err(GitError::Failed {
                command: display,
                status: output.status,
                stdout: output.stdout_str(),
                stderr: output.stderr_str(),
            });
        }
        Ok(output)
    }

    /// Run and return trimmed stdout. Nonzero exit is an error.
    pub fn stdout(self) -> Result<String, GitError> {
        Ok(self.run()?.stdout_trimmed())
    }

    /// Run and report whether git exited 0. Only spawn failures and
    /// timeouts are errors; any nonzero exit is `Ok(false)`.
    pub fn succeeds(self) -> Result<bool, GitError> {
        Ok(self.allow_fail().run()?.success())
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Default)]
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    pending: usize,
    error: Option<std::io::Error>,
}

impl Collected {
    /// Fold in streams that arrived in a later collection round.
    fn merge(&mut self, later: Collected) {
        if !later.stdout.is_empty() {
            self.stdout = later.stdout;
        }
        if !later.stderr.is_empty() {
            self.stderr = later.stderr;
        }
        if self.error.is_none() {
            self.error = later.error;
        }
        self.pending = later.pending;
    }
}

fn spawn_drain<R: Read + Send + 'static>(
    stream: Stream,
    mut reader: R,
    tx: mpsc::Sender<(Stream, std::io::Result<Vec<u8>>)>,
) {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let result = reader.read_to_end(&mut buf).map(|_| buf);
        let _ = tx.send((stream, result));
    });
}

fn collect(
    rx: &mpsc::Receiver<(Stream, std::io::Result<Vec<u8>>)>,
    mut pending: usize,
    deadline: Instant,
) -> Collected {
    let mut collected = Collected::default();
    while pending > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((stream, result)) => {
                pending -= 1;
                match (stream, result) {
                    (Stream::Stdout, Ok(bytes)) => collected.stdout = bytes,
                    (Stream::Stderr, Ok(bytes)) => collected.stderr = bytes,
                    (_, Err(error)) => {
                        collected.error.get_or_insert(error);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                pending = 0;
                break;
            }
        }
    }
    collected.pending = pending;
    collected
}

/// Poll the child until it exits or `deadline` passes (`Ok(None)`).
fn wait_until(child: &mut Child, deadline: Instant) -> std::io::Result<Option<ExitStatus>> {
    let mut backoff = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        std::thread::sleep(backoff.min(deadline - now));
        backoff = (backoff * 2).min(Duration::from_millis(25));
    }
}

/// Signal the child's process group (`grouped`) or just the child.
#[cfg(unix)]
fn signal_child(pid: u32, grouped: bool, signal: libc::c_int) {
    let Ok(id) = libc::pid_t::try_from(pid) else {
        return;
    };
    // Never signal pid/group 0 (our own group) or negative ids.
    if id <= 0 {
        return;
    }
    // SAFETY: kill/killpg have no memory-safety preconditions.
    unsafe {
        if grouped {
            // The child was spawned with `process_group(0)`, so its pid is
            // the process-group id.
            libc::killpg(id, signal);
        } else {
            libc::kill(id, signal);
        }
    }
}

/// Whether any process is still alive in group `pgid`.
#[cfg(unix)]
fn group_alive(pgid: u32) -> bool {
    let Ok(id) = libc::pid_t::try_from(pgid) else {
        return false;
    };
    if id <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence.
    let rc = unsafe { libc::killpg(id, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Stop a timed-out child: SIGTERM first so git can release `index.lock`
/// and clean up, then SIGKILL for whatever is still alive after `grace`.
/// Reaps the child; returns once the group (if any) is gone.
fn terminate_child(child: &mut Child, grouped: bool, grace: Duration) {
    #[cfg(unix)]
    {
        let pid = child.id();
        signal_child(pid, grouped, libc::SIGTERM);
        let deadline = Instant::now() + grace;
        loop {
            let exited = child.try_wait().ok().flatten().is_some();
            if exited && (!grouped || !group_alive(pid)) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        signal_child(pid, grouped, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = (grouped, grace);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Kill the child (and its whole process group when `grouped`) immediately
/// and reap it.
fn kill_group(child: &mut Child, grouped: bool) {
    kill_group_by_pid(child.id(), grouped);
    let _ = child.kill();
    let _ = child.wait();
}

/// SIGKILL the group led by `pid`. A no-op when the child shares our group:
/// there is no group of its own to kill, and the pid may already be reaped.
fn kill_group_by_pid(pid: u32, grouped: bool) {
    #[cfg(unix)]
    if grouped {
        signal_child(pid, true, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, grouped);
    }
}

/// After a killed command, remove `index.lock` if it is a leftover of that
/// command: modified at or after `spawned_at`, with no process left in the
/// command's group (`pgid`, when it ran in its own group). A lock that
/// predates the command, or one a live process in the group may still own, is
/// left alone. Returns whether a lock was removed.
fn remove_stale_index_lock(repo: &Path, spawned_at: SystemTime, pgid: Option<u32>) -> bool {
    #[cfg(unix)]
    if pgid.is_some_and(group_alive) {
        return false;
    }
    #[cfg(not(unix))]
    let _ = pgid;
    let mut probe = cmd(repo).args(["rev-parse", "--git-path", "index.lock"]);
    probe.skip_lock_cleanup = true;
    let Ok(raw) = probe.timeout(Duration::from_secs(10)).stdout() else {
        return false;
    };
    if raw.is_empty() {
        return false;
    }
    let lock = repo.join(raw);
    let Ok(modified) = std::fs::metadata(&lock).and_then(|meta| meta.modified()) else {
        return false;
    };
    if modified < spawned_at {
        return false;
    }
    match std::fs::remove_file(&lock) {
        Ok(()) => {
            tracing::warn!(
                lock = %lock.display(),
                "removed stale index.lock left by a killed git command"
            );
            true
        }
        Err(_) => false,
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        cmd(tmp.path())
            .args(["init", "-q", "-b", "main"])
            .run()
            .unwrap();
        cmd(tmp.path())
            .args(["config", "user.email", "git-runner@example.com"])
            .run()
            .unwrap();
        cmd(tmp.path())
            .args(["config", "user.name", "Git Runner"])
            .run()
            .unwrap();
        tmp
    }

    #[test]
    fn program_is_resolved_once_and_runs() {
        let first = program();
        let second = program();
        assert!(std::ptr::eq(first, second));
        let out = cmd_global().arg("--version").run().unwrap();
        assert!(out.stdout_str().starts_with("git version"));
    }

    #[test]
    fn nonzero_exit_is_failed_error_with_stderr() {
        let tmp = init_repo();
        let error = cmd(tmp.path())
            .args(["rev-parse", "--verify", "refs/heads/does-not-exist"])
            .run()
            .unwrap_err();
        match &error {
            GitError::Failed { status, stderr, .. } => {
                assert!(!status.success());
                assert!(stderr.contains("fatal"), "stderr: {stderr}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(error.to_string().contains("rev-parse"));
        assert!(!error.stderr().is_empty());
    }

    #[test]
    fn allow_fail_returns_output_for_nonzero_exit() {
        let tmp = init_repo();
        let out = cmd(tmp.path())
            .args(["show-ref", "--verify", "--quiet", "refs/heads/nope"])
            .allow_fail()
            .run()
            .unwrap();
        assert_eq!(out.code(), Some(1));
        assert!(
            !cmd(tmp.path())
                .args(["show-ref", "--verify", "--quiet", "refs/heads/nope"])
                .succeeds()
                .unwrap()
        );
    }

    #[test]
    fn missing_repo_dir_is_failed_not_spawn() {
        let error = cmd("/tmp/__batty_git_runner_missing__")
            .arg("status")
            .run()
            .unwrap_err();
        assert!(matches!(error, GitError::Failed { .. }), "{error:?}");
    }

    #[test]
    fn large_output_on_both_streams_does_not_block() {
        let tmp = init_repo();
        // > 64 KiB on stdout and stderr simultaneously via a git alias that
        // shells out; a runner that reads the streams sequentially deadlocks.
        let script = "!f() { head -c 400000 /dev/zero | tr '\\0' o; head -c 400000 /dev/zero | tr '\\0' e 1>&2; }; f";
        let out = cmd(tmp.path())
            .args(["-c", &format!("alias.spew={script}"), "spew"])
            .timeout(Duration::from_secs(30))
            .run()
            .unwrap();
        assert_eq!(out.stdout.len(), 400000);
        assert_eq!(out.stderr.len(), 400000);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_process_group() {
        let tmp = init_repo();
        let marker = tmp.path().join("grandchild-survived");
        // The alias spawns a grandchild that would write the marker after 3s.
        let script = format!(
            "!f() {{ (sleep 3; touch '{}') & sleep 30; }}; f",
            marker.display()
        );
        let started = Instant::now();
        let error = cmd(tmp.path())
            .args(["-c", &format!("alias.hang={script}"), "hang"])
            .timeout(Duration::from_millis(300))
            .run()
            .unwrap_err();
        assert!(matches!(error, GitError::Timeout { .. }), "{error:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !marker.exists(),
            "grandchild outlived the timeout kill of its process group"
        );
    }

    #[cfg(unix)]
    #[test]
    fn leftover_descendant_holding_pipe_does_not_stall_the_runner() {
        let tmp = init_repo();
        // The backgrounded sleep inherits git's stdout pipe and outlives git.
        let script = "!f() { echo hi; sleep 30 & }; f";
        let started = Instant::now();
        let out = cmd(tmp.path())
            .args(["-c", &format!("alias.linger={script}"), "linger"])
            .timeout(Duration::from_secs(60))
            .run()
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "runner waited {:?} on a lingering pipe holder",
            started.elapsed()
        );
        assert_eq!(out.stdout_str(), "hi\n");
    }

    #[test]
    fn env_hygiene_removes_ambient_repo_vars_and_disables_prompts() {
        let command = cmd("/repo").arg("status").build(true);
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for removed in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            assert!(
                envs.contains(&(removed.to_string(), None)),
                "{removed} not removed: {envs:?}"
            );
        }
        assert!(envs.contains(&("GIT_TERMINAL_PROMPT".to_string(), Some("0".to_string()))));
        assert!(envs.contains(&("GIT_EDITOR".to_string(), Some("true".to_string()))));
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-C", "/repo", "status"]);
    }

    #[test]
    fn live_child_sees_noninteractive_env() {
        let tmp = init_repo();
        let alias = "!f() { printf '%s|%s|%s' \"${GIT_INDEX_FILE-unset}\" \"$GIT_TERMINAL_PROMPT\" \"$GIT_EDITOR\"; }; f";
        let out = cmd(tmp.path())
            .args(["-c", &format!("alias.envdump={alias}"), "envdump"])
            .run()
            .unwrap();
        assert_eq!(out.stdout_str(), "unset|0|true");
    }

    #[test]
    fn explicit_env_overrides_hygiene_defaults() {
        let tmp = init_repo();
        let alias = "!f() { printf '%s' \"$GIT_EDITOR\"; }; f";
        let out = cmd(tmp.path())
            .args(["-c", &format!("alias.ed={alias}"), "ed"])
            .env("GIT_EDITOR", "custom-editor")
            .run()
            .unwrap();
        assert_eq!(out.stdout_str(), "custom-editor");
    }

    #[test]
    fn long_running_subcommands_get_longer_default_timeout() {
        assert_eq!(cmd(".").arg("fetch").effective_timeout(), LONG_TIMEOUT);
        assert_eq!(
            cmd(".")
                .args(["-c", "x=y", "push", "origin"])
                .effective_timeout(),
            LONG_TIMEOUT
        );
        assert_eq!(cmd(".").arg("commit").effective_timeout(), LONG_TIMEOUT);
        assert_eq!(cmd(".").arg("status").effective_timeout(), LONG_TIMEOUT);
        assert_eq!(
            cmd(".").arg("rev-parse").effective_timeout(),
            DEFAULT_TIMEOUT
        );
        assert_eq!(
            cmd(".")
                .arg("fetch")
                .timeout(Duration::from_secs(1))
                .effective_timeout(),
            Duration::from_secs(1)
        );
    }

    fn args_of(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_class_mapping() {
        for sub in [
            "reset",
            "stash",
            "add",
            "status",
            "clean",
            "checkout",
            "diff",
            "merge-tree",
            "fetch",
            "commit",
        ] {
            assert!(is_long_running(Some(sub)), "{sub} should be long");
        }
        for sub in [
            "rev-parse",
            "rev-list",
            "for-each-ref",
            "config",
            "show-ref",
        ] {
            assert!(!is_long_running(Some(sub)), "{sub} should be short");
        }
        assert!(!is_long_running(None));
        // Global options before the subcommand are skipped.
        let args = args_of(&["-c", "core.x=1", "--no-pager", "reset", "--hard"]);
        assert_eq!(subcommand_of(&args).as_deref(), Some("reset"));
        assert_eq!(subcommand_of(&[]), None);
    }

    #[test]
    fn effective_timeout_uses_long_class_for_reset_and_short_for_rev_parse() {
        assert_eq!(
            cmd("/r").args(["reset", "--hard"]).effective_timeout(),
            LONG_TIMEOUT
        );
        assert_eq!(
            cmd("/r").args(["rev-parse", "HEAD"]).effective_timeout(),
            DEFAULT_TIMEOUT
        );
        assert_eq!(
            cmd("/r")
                .args(["reset", "--hard"])
                .timeout(Duration::from_secs(7))
                .effective_timeout(),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn process_group_decision_depends_on_tty_and_subcommand() {
        for sub in ["commit", "merge", "rebase", "tag", "push"] {
            assert!(!use_own_process_group(true, Some(sub)), "{sub} on a tty");
            assert!(use_own_process_group(false, Some(sub)), "{sub} headless");
        }
        for sub in ["status", "rev-parse", "reset", "fetch"] {
            assert!(use_own_process_group(true, Some(sub)), "{sub} on a tty");
            assert!(use_own_process_group(false, Some(sub)), "{sub} headless");
        }
        assert!(use_own_process_group(true, None));
        assert!(use_own_process_group(false, None));
    }

    #[cfg(unix)]
    fn spawn_grouped(script: &str) -> Child {
        use std::os::unix::process::CommandExt;
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn terminate_child_sigterm_is_enough_for_a_cooperative_process() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = spawn_grouped("sleep 30");
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        terminate_child(&mut child, true, Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(4));
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    #[cfg(unix)]
    #[test]
    fn terminate_child_escalates_to_sigkill_after_the_grace_period() {
        use std::os::unix::process::ExitStatusExt;
        // Ignores SIGTERM (the ignore is inherited by `sleep`).
        let marker = tempfile::tempdir().unwrap();
        let ready = marker.path().join("ready");
        let mut child = spawn_grouped(&format!(
            "trap '' TERM; : > '{}'; while :; do sleep 1; done",
            ready.display()
        ));
        let pid = child.id();
        // Wait until the trap is installed, or SIGTERM would just kill the shell.
        assert!(crate::team::test_support::wait_until(
            Duration::from_secs(30),
            || ready.exists()
        ));
        let grace = Duration::from_millis(600);
        let started = Instant::now();
        terminate_child(&mut child, true, grace);
        assert!(
            started.elapsed() >= grace,
            "SIGKILL must wait out the grace period"
        );
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(
            crate::team::test_support::wait_until(Duration::from_secs(10), || !group_alive(pid)),
            "whole group must be gone"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_index_lock_from_killed_command_is_removed() {
        let tmp = init_repo();
        let lock = tmp.path().join(".git/index.lock");
        let spawned_at = SystemTime::now() - Duration::from_secs(5);
        std::fs::write(&lock, "").unwrap();
        // pgid of an already-dead group.
        let mut dead = spawn_grouped("true");
        let dead_pid = dead.id();
        dead.wait().unwrap();
        assert!(remove_stale_index_lock(
            tmp.path(),
            spawned_at,
            Some(dead_pid)
        ));
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn index_lock_owned_by_a_live_process_or_older_than_the_command_is_kept() {
        let tmp = init_repo();
        let lock = tmp.path().join(".git/index.lock");
        std::fs::write(&lock, "").unwrap();

        // A live process remains in the group: do not touch the lock.
        let mut live = spawn_grouped("sleep 30");
        assert!(!remove_stale_index_lock(
            tmp.path(),
            SystemTime::now() - Duration::from_secs(5),
            Some(live.id())
        ));
        assert!(lock.exists());
        signal_child(live.id(), true, libc::SIGKILL);
        live.wait().unwrap();

        // The lock predates the command: it is someone else's.
        assert!(!remove_stale_index_lock(
            tmp.path(),
            SystemTime::now() + Duration::from_secs(60),
            None
        ));
        assert!(lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_removes_index_lock_created_by_the_killed_command() {
        let tmp = init_repo();
        let lock = tmp.path().join(".git/index.lock");
        // The aliased "git" creates the lock then hangs, like a killed `add`.
        let script = format!("!f() {{ touch '{}'; sleep 30; }}; f", lock.display());
        let error = cmd(tmp.path())
            .args(["-c", &format!("alias.hang={script}"), "hang"])
            .timeout(Duration::from_millis(500))
            .run()
            .unwrap_err();
        assert!(matches!(error, GitError::Timeout { .. }), "{error:?}");
        assert!(!lock.exists(), "stale index.lock left behind after kill");
    }

    #[test]
    fn display_includes_repo_and_args() {
        assert_eq!(
            cmd("/my/repo").args(["status", "--porcelain"]).display(),
            "git -C /my/repo status --porcelain"
        );
    }
}
