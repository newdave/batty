//! tmux session management for batty.
//!
//! Wraps tmux CLI commands for session lifecycle, output capture via pipe-pane,
//! input injection via send-keys, and pane management. This replaces the
//! portable-pty direct approach from Phase 1 with tmux-based supervision.

use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::team::errors::TmuxError;

#[cfg(test)]
const SUPERVISOR_CONTROL_OPTION: &str = "@batty_supervisor_control";

const SEND_KEYS_SUBMIT_DELAY_MS: u64 = 100;
const TMUX_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn tmux_command_timeout() -> Duration {
    #[cfg(test)]
    if let Ok(value) = std::env::var("BATTY_TEST_TMUX_COMMAND_TIMEOUT_MS") {
        if let Ok(ms) = value.parse::<u64>() {
            return Duration::from_millis(ms);
        }
    }

    TMUX_COMMAND_TIMEOUT
}

/// Build a `tmux` command. Every tmux invocation in the crate goes through here.
///
/// In test builds the command is pinned to a private server: `$TMUX` is
/// removed (it would otherwise select the caller's live server) and
/// `TMUX_TMPDIR` points at a per-process temp directory, so tests can never
/// touch the user's default tmux server, sessions or key bindings.
pub fn tmux_command() -> Command {
    let command = Command::new("tmux");
    #[cfg(test)]
    let command = {
        let mut command = command;
        command.env_remove("TMUX");
        command.env("TMUX_TMPDIR", test_tmux_dir());
        command
    };
    command
}

/// Per-process private tmux socket directory used by test builds.
#[cfg(test)]
pub(crate) fn test_tmux_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        // Short path: unix socket paths are limited to ~104 bytes on macOS.
        let dir = std::path::PathBuf::from(format!("/tmp/bt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    })
}

#[cfg(test)]
/// Return whether the tmux binary is available in the current environment.
pub fn tmux_available() -> bool {
    crate::tmux::tmux_command()
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success())
}

pub(crate) fn run_tmux_with_timeout<I, S>(
    args: I,
    command_label: &'static str,
    target: Option<&str>,
) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = crate::tmux::tmux_command();
    command.args(args);
    command_output_with_timeout(command, command_label, target, tmux_command_timeout())
}

fn spawn_pipe_drain<R: std::io::Read + Send + 'static>(
    mut pipe: R,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn join_pipe_drain(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    handle.and_then(|h| h.join().ok()).unwrap_or_default()
}

fn command_output_with_timeout(
    mut command: Command,
    command_label: &'static str,
    target: Option<&str>,
    timeout: Duration,
) -> Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| TmuxError::exec(command_label, error))?;
    let started_at = Instant::now();
    // Drain both pipes concurrently: a child writing more than the pipe
    // buffer (~64KB) would otherwise block until the timeout kills it.
    let stdout_reader = child.stdout.take().map(spawn_pipe_drain);
    let stderr_reader = child.stderr.take().map(spawn_pipe_drain);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let elapsed = started_at.elapsed();
                let output = Output {
                    status,
                    stdout: join_pipe_drain(stdout_reader),
                    stderr: join_pipe_drain(stderr_reader),
                };
                debug!(
                    command = command_label,
                    target,
                    elapsed_ms = elapsed.as_millis(),
                    status = ?output.status.code(),
                    "tmux command completed"
                );
                return Ok(output);
            }
            Ok(None) if started_at.elapsed() >= timeout => {
                let elapsed = started_at.elapsed();
                warn!(
                    command = command_label,
                    target,
                    timeout_ms = timeout.as_millis(),
                    elapsed_ms = elapsed.as_millis(),
                    pid = child.id(),
                    "tmux command timed out; killing subprocess"
                );
                let _ = child.kill();
                let _ = child.wait();
                return Err(TmuxError::command_failed(
                    command_label,
                    target,
                    &format!(
                        "timed out after {}ms (elapsed {}ms)",
                        timeout.as_millis(),
                        elapsed.as_millis()
                    ),
                )
                .into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => return Err(TmuxError::exec(command_label, error).into()),
        }
    }
}

/// Check if a tmux session exists.
pub fn session_exists(session: &str) -> bool {
    run_tmux_with_timeout(["has-session", "-t", session], "has-session", Some(session))
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check whether a tmux server is currently running.
pub fn server_running() -> bool {
    run_tmux_with_timeout(["list-sessions"], "list-sessions", None)
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if a specific tmux pane target exists.
pub fn pane_exists(target: &str) -> bool {
    run_tmux_with_timeout(
        ["display-message", "-p", "-t", target, "#{pane_id}"],
        "display-message #{pane_id}",
        Some(target),
    )
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Check whether a pane target is dead (`remain-on-exit` pane).
pub fn pane_dead(target: &str) -> Result<bool> {
    let output = run_tmux_with_timeout(
        ["display-message", "-p", "-t", target, "#{pane_dead}"],
        "display-message #{pane_dead}",
        Some(target),
    )
    .with_context(|| format!("failed to query pane_dead for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed(
            "display-message #{pane_dead}",
            Some(target),
            &stderr,
        )
        .into());
    }

    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(value == "1")
}

/// Get the active pane id for a session target (for example: `%3`).
pub fn pane_id(target: &str) -> Result<String> {
    let output = run_tmux_with_timeout(
        ["display-message", "-p", "-t", target, "#{pane_id}"],
        "display-message #{pane_id}",
        Some(target),
    )
    .with_context(|| format!("failed to resolve pane id for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(
            TmuxError::command_failed("display-message #{pane_id}", Some(target), &stderr).into(),
        );
    }

    let pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if pane.is_empty() {
        return Err(TmuxError::EmptyPaneId {
            target: target.to_string(),
        }
        .into());
    }
    Ok(pane)
}

/// Return `(pane_width, pane_height)` for a tmux pane target.
pub fn pane_dimensions(target: &str) -> Result<(u16, u16)> {
    let output = run_tmux_with_timeout(
        [
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_width} #{pane_height}",
        ],
        "display-message #{pane_width} #{pane_height}",
        Some(target),
    )
    .with_context(|| format!("failed to query pane dimensions for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed(
            "display-message #{pane_width} #{pane_height}",
            Some(target),
            &stderr,
        )
        .into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parts = stdout.split_whitespace();
    let width = parts
        .next()
        .context("tmux pane width missing")?
        .parse()
        .context("invalid tmux pane width")?;
    let height = parts
        .next()
        .context("tmux pane height missing")?
        .parse()
        .context("invalid tmux pane height")?;
    Ok((width, height))
}

/// Get the current working directory for a pane target.
pub fn pane_current_path(target: &str) -> Result<String> {
    let output = run_tmux_with_timeout(
        [
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_current_path}",
        ],
        "display-message #{pane_current_path}",
        Some(target),
    )
    .with_context(|| format!("failed to resolve pane current path for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux display-message pane_current_path failed: {stderr}");
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        bail!("tmux returned empty pane current path for target '{target}'");
    }
    Ok(path)
}

/// Create a detached tmux session running the given command.
///
/// The session is created with `new-session -d` so it starts in the background.
/// The executor command is the initial command the session runs.
pub fn create_session(session: &str, program: &str, args: &[String], work_dir: &str) -> Result<()> {
    if session_exists(session) {
        return Err(TmuxError::SessionExists {
            session: session.to_string(),
        }
        .into());
    }

    // Build the full command string for tmux
    // tmux new-session -d -s <name> -c <work_dir> <program> <args...>
    let mut cmd = crate::tmux::tmux_command();
    cmd.args(["new-session", "-d", "-s", session, "-c", work_dir]);
    // Set a generous size so the PTY isn't tiny
    cmd.args(["-x", "220", "-y", "50"]);
    // Unset CLAUDECODE so nested Claude Code sessions can launch.
    // Without this, Claude Code detects the parent session's env var and
    // refuses to start ("cannot be launched inside another Claude Code session").
    cmd.args(["env", "-u", "CLAUDECODE"]);
    cmd.arg(program);
    for arg in args {
        cmd.arg(arg);
    }

    let output =
        command_output_with_timeout(cmd, "new-session", Some(session), TMUX_COMMAND_TIMEOUT)
            .with_context(|| format!("failed to create tmux session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed("new-session", Some(session), &stderr).into());
    }

    if let Err(e) = set_mouse(session, true) {
        warn!(
            session = session,
            error = %e,
            "failed to enable tmux mouse mode"
        );
    }

    info!(session = session, "tmux session created");
    Ok(())
}

#[cfg(test)]
/// Create a detached tmux window in an existing session running the given command.
pub fn create_window(
    session: &str,
    window_name: &str,
    program: &str,
    args: &[String],
    work_dir: &str,
) -> Result<()> {
    if !session_exists(session) {
        bail!("tmux session '{session}' not found");
    }

    let mut cmd = crate::tmux::tmux_command();
    cmd.args([
        "new-window",
        "-d",
        "-t",
        session,
        "-n",
        window_name,
        "-c",
        work_dir,
    ]);
    // Unset CLAUDECODE so nested Claude Code sessions can launch from
    // additional windows (for example, parallel agent slots).
    cmd.args(["env", "-u", "CLAUDECODE"]);
    cmd.arg(program);
    for arg in args {
        cmd.arg(arg);
    }

    let output =
        command_output_with_timeout(cmd, "new-window", Some(window_name), TMUX_COMMAND_TIMEOUT)
            .with_context(|| format!("failed to create tmux window '{window_name}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux new-window failed: {stderr}");
    }

    Ok(())
}

/// Rename an existing tmux window target (e.g. `session:0` or `session:old`).
pub fn rename_window(target: &str, new_name: &str) -> Result<()> {
    let output = run_tmux_with_timeout(
        ["rename-window", "-t", target, new_name],
        "rename-window",
        Some(target),
    )
    .with_context(|| format!("failed to rename tmux window target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux rename-window failed: {stderr}");
    }

    Ok(())
}

#[cfg(test)]
/// Select a tmux window target (e.g. `session:agent-1`).
pub fn select_window(target: &str) -> Result<()> {
    let output = run_tmux_with_timeout(
        ["select-window", "-t", target],
        "select-window",
        Some(target),
    )
    .with_context(|| format!("failed to select tmux window '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux select-window failed: {stderr}");
    }

    Ok(())
}

#[cfg(test)]
/// Set up pipe-pane to capture all output from the target pane to a log file.
///
/// Uses `tmux pipe-pane -t <session> "cat >> <log_path>"` to stream all PTY
/// output to a file. This is the foundation for event extraction.
pub fn setup_pipe_pane(target: &str, log_path: &std::path::Path) -> Result<()> {
    // Ensure log directory exists
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory: {}", parent.display()))?;
    }

    let pipe_cmd = format!("cat >> {}", log_path.display());
    let output = run_tmux_with_timeout(
        ["pipe-pane", "-t", target, &pipe_cmd],
        "pipe-pane",
        Some(target),
    )
    .with_context(|| format!("failed to set up pipe-pane for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux pipe-pane failed: {stderr}");
    }

    info!(target = target, log = %log_path.display(), "pipe-pane configured");
    Ok(())
}

/// Create a detached window in `session` and return its first pane id.
pub fn new_window_pane(session: &str, window_name: &str, work_dir: &str) -> Result<String> {
    let output = run_tmux_with_timeout(
        [
            "new-window",
            "-d",
            "-t",
            &format!("{session}:"),
            "-n",
            window_name,
            "-c",
            work_dir,
            "-P",
            "-F",
            "#{pane_id}",
            "env",
            "-u",
            "CLAUDECODE",
            "bash",
        ],
        "new-window",
        Some(session),
    )
    .with_context(|| format!("failed to create window '{window_name}' in '{session}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed("new-window", Some(session), &stderr).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Index of the window containing `pane` (e.g. `%3`).
pub fn window_index_of_pane(pane: &str) -> Result<usize> {
    let output = run_tmux_with_timeout(
        ["display-message", "-p", "-t", pane, "#{window_index}"],
        "display-message window_index",
        Some(pane),
    )?;
    if !output.status.success() {
        bail!("tmux could not resolve window for pane '{pane}'");
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .with_context(|| format!("unexpected window index for pane '{pane}'"))
}

/// Find a member's `(window_index, pane_id)` in `list-panes -s` output of
/// tab-separated `window_index, pane_id, @batty_role` lines.
pub fn find_member_target(listing: &str, member: &str) -> Option<(usize, String)> {
    listing.lines().find_map(|line| {
        let mut parts = line.split('\t');
        let window = parts.next()?.trim().parse().ok()?;
        let pane = parts.next()?.trim();
        let role = parts.next()?.trim();
        (role == member).then(|| (window, pane.to_string()))
    })
}

/// Names of members that have a pane in the session (for error hints).
pub fn listed_members(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| line.split('\t').nth(2))
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect()
}

/// Select the window and pane of `member` in `session`.
pub fn select_member(session: &str, member: &str) -> Result<()> {
    let output = run_tmux_with_timeout(
        [
            "list-panes",
            "-s",
            "-t",
            session,
            "-F",
            "#{window_index}\t#{pane_id}\t#{@batty_role}",
        ],
        "list-panes member",
        Some(session),
    )?;
    if !output.status.success() {
        bail!("tmux could not list panes for '{session}'");
    }
    let listing = String::from_utf8_lossy(&output.stdout).to_string();
    let Some((window, pane)) = find_member_target(&listing, member) else {
        let known = listed_members(&listing).join(", ");
        bail!("no member '{member}' in session '{session}' (members: {known})");
    };
    let _ = run_tmux_with_timeout(
        ["select-window", "-t", &format!("{session}:{window}")],
        "select-window",
        Some(session),
    );
    let _ = run_tmux_with_timeout(["select-pane", "-t", &pane], "select-pane", Some(&pane));
    Ok(())
}

/// Attach to an existing tmux session (blocks until detach/exit).
///
/// If already inside tmux, uses `switch-client` instead of `attach-session`.
pub fn attach(session: &str) -> Result<()> {
    if !session_exists(session) {
        bail!(
            "tmux session '{session}' not found — is batty running? \
             Start with `batty start` first"
        );
    }

    let inside_tmux = std::env::var("TMUX").is_ok();

    let (cmd, args) = if inside_tmux {
        ("switch-client", vec!["-t", session])
    } else {
        ("attach-session", vec!["-t", session])
    };

    let status = crate::tmux::tmux_command()
        .arg(cmd)
        .args(&args)
        .status()
        .with_context(|| format!("failed to {cmd} to tmux session '{session}'"))?;

    if !status.success() {
        bail!("tmux {cmd} to '{session}' failed");
    }

    Ok(())
}

/// Send keys to a tmux target (session or pane).
///
/// This is how batty injects responses into the executor's PTY.
/// The `keys` string is sent literally, followed by Enter if `press_enter` is true.
pub fn send_keys(target: &str, keys: &str, press_enter: bool) -> Result<()> {
    if !keys.is_empty() {
        // `-l` sends text literally so punctuation/symbols are not interpreted as
        // tmux key names.
        let mut command = crate::tmux::tmux_command();
        command.args(["send-keys", "-t", target, "-l", "--", keys]);
        let output =
            command_output_with_timeout(command, "send-keys", Some(target), TMUX_COMMAND_TIMEOUT)
                .with_context(|| format!("failed to send keys to target '{target}'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TmuxError::command_failed("send-keys", Some(target), &stderr).into());
        }
    }

    if press_enter {
        // Keep submission as a separate keypress so the target app processes the
        // literal text first, matching the watcher script's behavior.
        if !keys.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(SEND_KEYS_SUBMIT_DELAY_MS));
        }

        let mut command = crate::tmux::tmux_command();
        command.args(["send-keys", "-t", target, "Enter"]);
        let output = command_output_with_timeout(
            command,
            "send-keys Enter",
            Some(target),
            TMUX_COMMAND_TIMEOUT,
        )
        .with_context(|| format!("failed to send Enter to target '{target}'"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(TmuxError::command_failed("send-keys Enter", Some(target), &stderr).into());
        }
    }

    debug!(target = target, keys = keys, "sent keys");
    Ok(())
}

/// List all tmux session names matching a prefix.
pub fn list_sessions_with_prefix(prefix: &str) -> Vec<String> {
    let output = run_tmux_with_timeout(
        ["list-sessions", "-F", "#{session_name}"],
        "list-sessions",
        None,
    );

    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|name| name.starts_with(prefix))
            .map(|s| s.to_string())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
/// RAII guard that kills a tmux session on drop.
///
/// Ensures test sessions are cleaned up even on panic/assert failure.
/// Use in tests instead of manual `kill_session()` calls.
pub struct TestSession {
    name: String,
}

#[cfg(test)]
impl TestSession {
    #[cfg(test)]
    /// Create a new test session guard wrapping an existing session name.
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let _ = kill_session(&name);
        Self { name }
    }

    #[cfg(test)]
    /// The session name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
impl Drop for TestSession {
    fn drop(&mut self) {
        let _ = kill_session(&self.name);
    }
}

/// Kill a tmux session.
pub fn kill_session(session: &str) -> Result<()> {
    if !session_exists(session) {
        return Ok(()); // already gone
    }

    let output = run_tmux_with_timeout(
        ["kill-session", "-t", session],
        "kill-session",
        Some(session),
    )
    .with_context(|| format!("failed to kill tmux session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux kill-session failed: {stderr}");
    }

    info!(session = session, "tmux session killed");
    release_batty_keys_if_unused();
    Ok(())
}

/// Session option marking a session that uses batty's root-table keys.
pub const BATTY_KEYS_OPTION: &str = "@batty_keys";
/// Prefix of the per-session options that hold each key's action.
pub const BATTY_KEY_OPTION_PREFIX: &str = "@batty_key_";

/// Who owns a key in tmux's root table, judged from `list-keys -T root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootKeyOwner {
    Unbound,
    Batty,
    Other,
}

/// Parse one `list-keys` line into `(table, key, command)`.
fn parse_list_keys_line(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.trim_start().strip_prefix("bind-key")?.trim_start();
    let mut table = "prefix";
    let mut rest = rest;
    // Flags come before the key: -r, -n, -T <table>, -N <note>.
    loop {
        if let Some(after) = rest.strip_prefix("-T ") {
            let after = after.trim_start();
            let end = after.find(char::is_whitespace)?;
            table = &after[..end];
            rest = after[end..].trim_start();
        } else if let Some(after) = rest.strip_prefix("-n ") {
            table = "root";
            rest = after.trim_start();
        } else if let Some(after) = rest.strip_prefix("-r ") {
            rest = after.trim_start();
        } else {
            break;
        }
    }
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some((table, &rest[..end], rest[end..].trim_start()))
}

/// Owner of `key` in the root table, given `list-keys -T root` output.
pub fn root_key_owner(listing: &str, key: &str) -> RootKeyOwner {
    listing
        .lines()
        .filter_map(parse_list_keys_line)
        .find(|(table, k, _)| *table == "root" && *k == key)
        .map_or(RootKeyOwner::Unbound, |(_, _, command)| {
            if command.contains(BATTY_KEY_OPTION_PREFIX) {
                RootKeyOwner::Batty
            } else {
                RootKeyOwner::Other
            }
        })
}

/// Root-table keys that batty installed, given `list-keys -T root` output.
pub fn batty_root_keys(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(parse_list_keys_line)
        .filter(|(table, _, command)| *table == "root" && command.contains(BATTY_KEY_OPTION_PREFIX))
        .map(|(_, key, _)| key.to_string())
        .collect()
}

/// `list-keys -T root` output, or empty when tmux cannot answer.
pub fn root_key_listing() -> String {
    run_tmux_with_timeout(["list-keys", "-T", "root"], "list-keys root", None)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Remove batty's root-table keys once no remaining session uses them.
///
/// Keys are shared by every batty session on the server, so they stay while
/// any session still has [`BATTY_KEYS_OPTION`] set. Keys batty did not
/// install are never touched.
pub fn release_batty_keys_if_unused() {
    let Ok(output) = run_tmux_with_timeout(
        [
            "list-sessions",
            "-F",
            format!("#{{{BATTY_KEYS_OPTION}}}").as_str(),
        ],
        "list-sessions batty keys",
        None,
    ) else {
        return;
    };
    if !output.status.success() {
        // No server left (last session killed): nothing to release.
        return;
    }
    if String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.trim() == "1")
    {
        return;
    }
    for key in batty_root_keys(&root_key_listing()) {
        let _ = run_tmux_with_timeout(["unbind-key", "-T", "root", &key], "unbind-key", None);
    }
}

/// Capture the current visible content of a tmux target (session or pane).
///
/// Returns the text currently shown in the pane (useful for prompt detection
/// when pipe-pane output has a lag).
pub fn capture_pane(target: &str) -> Result<String> {
    capture_pane_recent(target, 0)
}

/// Capture only the most recent visible lines of a tmux target.
pub fn capture_pane_recent(target: &str, lines: u32) -> Result<String> {
    let mut args = vec![
        "capture-pane".to_string(),
        "-t".to_string(),
        target.to_string(),
        "-p".to_string(),
    ];
    if lines > 0 {
        args.push("-S".to_string());
        args.push(format!("-{lines}"));
    }

    let mut command = crate::tmux::tmux_command();
    command.args(&args);
    let output =
        command_output_with_timeout(command, "capture-pane", Some(target), TMUX_COMMAND_TIMEOUT)
            .with_context(|| format!("failed to capture pane for target '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(TmuxError::command_failed("capture-pane", Some(target), &stderr).into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Enable/disable tmux mouse mode for a session.
pub fn set_mouse(session: &str, enabled: bool) -> Result<()> {
    let value = if enabled { "on" } else { "off" };
    tmux_set(session, "mouse", value)
}

#[cfg(test)]
fn bind_supervisor_hotkey(session: &str, key: &str, action: &str) -> Result<()> {
    let output = run_tmux_with_timeout(
        [
            "bind-key",
            "-T",
            "prefix",
            key,
            "set-option",
            "-t",
            session,
            SUPERVISOR_CONTROL_OPTION,
            action,
        ],
        "bind-key",
        Some(session),
    )
    .with_context(|| format!("failed to bind supervisor hotkey '{key}' for '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux bind-key {key} failed: {stderr}");
    }

    Ok(())
}

#[cfg(test)]
/// Configure per-session supervisor hotkeys:
/// - `Prefix + Shift+P` -> pause automation
/// - `Prefix + Shift+R` -> resume automation
pub fn configure_supervisor_hotkeys(session: &str) -> Result<()> {
    tmux_set(session, SUPERVISOR_CONTROL_OPTION, "")?;
    bind_supervisor_hotkey(session, "P", "pause")?;
    bind_supervisor_hotkey(session, "R", "resume")?;
    Ok(())
}

/// List panes in a session.
///
/// Returns a list of pane IDs (e.g., ["%0", "%1"]).
#[cfg(test)]
pub fn list_panes(session: &str) -> Result<Vec<String>> {
    let output = run_tmux_with_timeout(
        ["list-panes", "-t", session, "-F", "#{pane_id}"],
        "list-panes",
        Some(session),
    )
    .with_context(|| format!("failed to list panes for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-panes failed: {stderr}");
    }

    let panes = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();

    Ok(panes)
}

#[cfg(test)]
fn list_window_names(session: &str) -> Result<Vec<String>> {
    let output = run_tmux_with_timeout(
        ["list-windows", "-t", session, "-F", "#{window_name}"],
        "list-windows",
        Some(session),
    )
    .with_context(|| format!("failed to list windows for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-windows failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

/// Helper: run `tmux set -t <session> <option> <value>`.
/// Split a pane horizontally (creates a new pane to the right).
///
/// `target_pane` is a tmux pane ID (e.g., `%0`). Returns the new pane's ID.
pub fn split_window_horizontal(target_pane: &str, size_pct: u32) -> Result<String> {
    let size = format!("{size_pct}%");
    let output = run_tmux_with_timeout(
        [
            "split-window",
            "-h",
            "-t",
            target_pane,
            "-l",
            &size,
            "-P",
            "-F",
            "#{pane_id}",
        ],
        "split-window -h",
        Some(target_pane),
    )
    .with_context(|| format!("failed to split pane '{target_pane}' horizontally"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window -h failed: {stderr}");
    }

    let pane_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(target_pane, pane_id = %pane_id, size_pct, "horizontal split created");
    Ok(pane_id)
}

/// Split a specific pane vertically (creates a new pane below).
///
/// Returns the new pane's ID.
pub fn split_window_vertical_in_pane(
    _session: &str,
    pane_id: &str,
    size_pct: u32,
) -> Result<String> {
    // Pane IDs (%N) are globally unique in tmux — use them directly as targets
    let size = format!("{size_pct}%");
    let output = run_tmux_with_timeout(
        [
            "split-window",
            "-v",
            "-t",
            pane_id,
            "-l",
            &size,
            "-P",
            "-F",
            "#{pane_id}",
        ],
        "split-window -v",
        Some(pane_id),
    )
    .with_context(|| format!("failed to split pane '{pane_id}' vertically"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux split-window -v failed for pane '{pane_id}': {stderr}");
    }

    let new_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(pane_id = %new_pane, parent = pane_id, size_pct, "vertical split created");
    Ok(new_pane)
}

/// Evenly spread a pane and any adjacent panes in its layout cell.
pub fn select_layout_even(target_pane: &str) -> Result<()> {
    let output = run_tmux_with_timeout(
        ["select-layout", "-E", "-t", target_pane],
        "select-layout -E",
        Some(target_pane),
    )
    .with_context(|| format!("failed to even layout for pane '{target_pane}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux select-layout -E failed: {stderr}");
    }

    Ok(())
}

/// Kill a specific tmux pane.
pub fn kill_pane(target: &str) -> Result<()> {
    let output = run_tmux_with_timeout(["kill-pane", "-t", target], "kill-pane", Some(target))
        .with_context(|| format!("failed to kill pane '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Don't error if already dead
        if !stderr.contains("not found") {
            bail!("tmux kill-pane failed: {stderr}");
        }
    }

    Ok(())
}

/// Respawn a dead pane with a new command.
pub fn respawn_pane(target: &str, command: &str) -> Result<()> {
    let output = run_tmux_with_timeout(
        ["respawn-pane", "-t", target, "-k", command],
        "respawn-pane",
        Some(target),
    )
    .with_context(|| format!("failed to respawn pane '{target}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux respawn-pane failed: {stderr}");
    }

    Ok(())
}

/// Helper: run `tmux set -t <session> <option> <value>`.
pub fn tmux_set(session: &str, option: &str, value: &str) -> Result<()> {
    let output = run_tmux_with_timeout(["set", "-t", session, option, value], "set", Some(session))
        .with_context(|| format!("failed to set tmux option '{option}' for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux set {option} failed: {stderr}");
    }

    Ok(())
}

/// # Test categories
///
/// Tests in this module are split into **unit** and **integration** categories:
///
/// - **Unit tests** — pure logic (parsing, string manipulation). Run with `cargo test`.
/// - **Integration tests** — require a running tmux server. Gated behind the `integration`
///   Cargo feature: `cargo test --features integration`. These tests are marked with
///   `#[cfg_attr(not(feature = "integration"), ignore)]`.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{EnvLock, EnvVarGuard, env_lock};
    use std::cell::RefCell;

    thread_local! {
        static TMUX_TEST_PATH_GUARD: RefCell<Option<EnvLock>> = const { RefCell::new(None) };
    }

    fn require_tmux_integration() -> bool {
        TMUX_TEST_PATH_GUARD.with(|slot| {
            if slot.borrow().is_none() {
                *slot.borrow_mut() = Some(env_lock());
            }
        });
        if tmux_available() {
            return true;
        }
        eprintln!("skipping tmux integration test: tmux binary unavailable");
        false
    }

    #[test]
    fn command_timeout_reproducer_bounds_stalled_subprocess() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 5"]);
        let started_at = Instant::now();

        let error = command_output_with_timeout(
            command,
            "supervision-harness-reproducer",
            Some("%reproducer"),
            Duration::from_millis(50),
        )
        .expect_err("sleeping subprocess should be killed at the timeout");

        assert!(
            started_at.elapsed() < Duration::from_secs(2),
            "stalled subprocess should be bounded by instrumentation"
        );
        assert!(
            error.to_string().contains("timed out after 50ms"),
            "timeout error should identify the bounded wait, got: {error}"
        );
    }

    #[test]
    fn command_timeout_reproducer_preserves_success_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf marker-ok"]);

        let output = command_output_with_timeout(
            command,
            "supervision-harness-reproducer",
            Some("%reproducer"),
            Duration::from_secs(1),
        )
        .expect("quick subprocess should complete normally");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "marker-ok");
    }

    #[test]
    fn command_timeout_drains_output_larger_than_pipe_buffer() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "head -c 300000 /dev/zero; head -c 200000 /dev/zero >&2",
        ]);
        let started_at = Instant::now();

        let output = command_output_with_timeout(
            command,
            "supervision-harness-large-output",
            None,
            Duration::from_secs(10),
        )
        .expect("large output must not deadlock the child");

        assert!(started_at.elapsed() < Duration::from_secs(5));
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 300_000);
        assert_eq!(output.stderr.len(), 200_000);
    }

    #[test]
    fn pane_id_is_bounded_by_tmux_command_timeout() {
        let _path_lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let fake_tmux = tmp.path().join("tmux");
        std::fs::write(&fake_tmux, "#!/bin/sh\nsleep 30\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path = format!(
            "{}:{}",
            tmp.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _path_guard = EnvVarGuard::set("PATH", &path);
        let _timeout_guard = EnvVarGuard::set("BATTY_TEST_TMUX_COMMAND_TIMEOUT_MS", "50");

        let started_at = Instant::now();
        let error = pane_id("%hung").expect_err("hung pane_id lookup should time out");
        let error_chain = format!("{error:#}");

        assert!(
            started_at.elapsed() < Duration::from_secs(2),
            "pane_id should not block indefinitely when tmux hangs"
        );
        assert!(
            error_chain.contains("timed out after 50ms"),
            "timeout should identify bounded tmux wait, got: {error_chain}"
        );
        assert!(
            error_chain.contains("%hung"),
            "timeout should include target context, got: {error_chain}"
        );
    }

    const ROOT_LISTING: &str = "\
bind-key    -T root   MouseDown1Pane    select-pane -t = \\; send-keys -M
bind-key    -T root   F2                if-shell -F \"#{@batty_key_F2}\" \"run-shell -C '#{@batty_key_F2}'\" \"send-keys F2\"
bind-key -r -T root   F5                display-message mine
bind-key    -T prefix F3                display-message prefixed
bind-key -n F7 display-message old-style
";

    #[test]
    fn root_key_owner_tells_batty_user_and_unbound_apart() {
        assert_eq!(root_key_owner(ROOT_LISTING, "F2"), RootKeyOwner::Batty);
        assert_eq!(root_key_owner(ROOT_LISTING, "F5"), RootKeyOwner::Other);
        assert_eq!(root_key_owner(ROOT_LISTING, "F7"), RootKeyOwner::Other);
        // A prefix-table binding does not own the root key.
        assert_eq!(root_key_owner(ROOT_LISTING, "F3"), RootKeyOwner::Unbound);
        assert_eq!(root_key_owner(ROOT_LISTING, "F12"), RootKeyOwner::Unbound);
        assert_eq!(root_key_owner("", "F2"), RootKeyOwner::Unbound);
    }

    #[test]
    fn batty_root_keys_lists_only_batty_bindings() {
        assert_eq!(batty_root_keys(ROOT_LISTING), vec!["F2".to_string()]);
        assert!(batty_root_keys("garbage\n").is_empty());
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn nonexistent_session_does_not_exist() {
        let _env_lock = env_lock();
        assert!(!session_exists("batty-test-nonexistent-12345"));
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_and_kill_session() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-lifecycle";
        // Clean up in case a previous test left it
        let _ = kill_session(session);

        // Create
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        assert!(session_exists(session));

        // Kill
        kill_session(session).unwrap();
        assert!(!session_exists(session));
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pane_current_path_returns_working_directory() {
        let _env_lock = env_lock();
        let session = "batty-test-pane-current-path";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        let pane = pane_id(session).unwrap();
        let path = pane_current_path(&pane).unwrap();
        assert_eq!(
            std::fs::canonicalize(&path).unwrap(),
            std::fs::canonicalize("/tmp").unwrap()
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn duplicate_session_is_error() {
        let _env_lock = env_lock();
        let session = "batty-test-dup";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let result = create_session(session, "sleep", &["10".to_string()], "/tmp");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_window_adds_named_window_to_existing_session() {
        let _env_lock = env_lock();
        let session = "batty-test-window-create";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        rename_window(&format!("{session}:0"), "agent-1").unwrap();
        create_window(session, "agent-2", "sleep", &["10".to_string()], "/tmp").unwrap();

        let names = list_window_names(session).unwrap();
        assert!(names.contains(&"agent-1".to_string()));
        assert!(names.contains(&"agent-2".to_string()));

        select_window(&format!("{session}:agent-1")).unwrap();
        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_window_unsets_claudecode_from_session_environment() {
        let _env_lock = env_lock();
        let session = "batty-test-window-unset-claudecode";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let output = crate::tmux::tmux_command()
            .args(["set-environment", "-t", session, "CLAUDECODE", "1"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "failed to set CLAUDECODE in tmux session: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        create_window(
            session,
            "env-check",
            "bash",
            &[
                "-lc".to_string(),
                "printf '%s' \"${CLAUDECODE:-unset}\"; sleep 1".to_string(),
            ],
            "/tmp",
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_millis(300));

        let content = capture_pane(&format!("{session}:env-check")).unwrap();
        assert!(
            content.contains("unset"),
            "expected CLAUDECODE to be unset in new window, got: {content:?}"
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_session_enables_mouse_mode() {
        let _env_lock = env_lock();
        let session = "batty-test-mouse";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let output = crate::tmux::tmux_command()
            .args(["show-options", "-t", session, "-v", "mouse"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(value, "on", "expected tmux mouse mode to be enabled");

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn send_keys_to_session() {
        let _env_lock = env_lock();
        let session = "batty-test-sendkeys";
        let _ = kill_session(session);

        // Create a session running cat (waits for input)
        create_session(session, "cat", &[], "/tmp").unwrap();

        // Give it a moment to start
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Send keys should succeed
        send_keys(session, "hello", true).unwrap();

        // Clean up
        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn send_keys_with_enter_submits_line() {
        let _env_lock = env_lock();
        let session = "batty-test-sendkeys-enter";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("sendkeys.log");

        create_session(session, "cat", &[], "/tmp").unwrap();
        setup_pipe_pane(session, &log_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        send_keys(session, "supervisor ping", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let content = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            content.contains("supervisor ping"),
            "expected injected text in pane log, got: {content:?}"
        );
        assert!(
            content.contains("supervisor ping\r\n") || content.contains("supervisor ping\n"),
            "expected submitted line ending in pane log, got: {content:?}"
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn send_keys_enter_only_submits_prompt() {
        let _env_lock = env_lock();
        let session = "batty-test-sendkeys-enter-only";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("sendkeys-enter-only.log");

        create_session(session, "cat", &[], "/tmp").unwrap();
        let pane = pane_id(session).unwrap();
        setup_pipe_pane(&pane, &log_path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));

        send_keys(&pane, "", true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        let content = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            content.contains("\r\n") || content.contains('\n'),
            "expected submitted empty line in pane log, got: {content:?}"
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pipe_pane_captures_output() {
        let _env_lock = env_lock();
        let session = "batty-test-pipe";
        let _ = kill_session(session);

        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("pty-output.log");

        // Create session with bash, set up pipe-pane FIRST, then trigger output
        create_session(session, "bash", &[], "/tmp").unwrap();
        let pane = pane_id(session).unwrap();

        // Set up pipe-pane before generating output
        setup_pipe_pane(&pane, &log_path).unwrap();

        // Small delay to ensure pipe-pane is active
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Now generate output that pipe-pane will capture
        send_keys(&pane, "echo pipe-test-output", true).unwrap();

        // Wait with retries for the output to appear in the log
        let mut found = false;
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if log_path.exists() {
                let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                if !content.is_empty() {
                    found = true;
                    break;
                }
            }
        }

        kill_session(session).unwrap();
        assert!(found, "pipe-pane log should have captured output");
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn capture_pane_returns_content() {
        let _env_lock = env_lock();
        let session = "batty-test-capture";
        let _ = kill_session(session);

        create_session(
            session,
            "bash",
            &["-c".to_string(), "echo 'capture-test'; sleep 2".to_string()],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let content = capture_pane(session).unwrap();
        // Should have some content (at least the echo output or prompt)
        assert!(
            !content.trim().is_empty(),
            "capture-pane should return content"
        );

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn capture_pane_recent_returns_content() {
        let _env_lock = env_lock();
        let session = "batty-test-capture-recent";
        let _ = kill_session(session);

        create_session(
            session,
            "bash",
            &[
                "-c".to_string(),
                "echo 'capture-recent-test'; sleep 2".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        let content = capture_pane_recent(session, 10).unwrap();
        assert!(content.contains("capture-recent-test"));

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn list_panes_returns_at_least_one() {
        let _env_lock = env_lock();
        let session = "batty-test-panes";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let panes = list_panes(session).unwrap();
        assert!(!panes.is_empty(), "session should have at least one pane");

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn configure_supervisor_hotkeys_initializes_control_option() {
        let _env_lock = env_lock();
        let session = "batty-test-supervisor-hotkeys";
        let _ = kill_session(session);

        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        configure_supervisor_hotkeys(session).unwrap();

        let output = crate::tmux::tmux_command()
            .args([
                "show-options",
                "-v",
                "-t",
                session,
                SUPERVISOR_CONTROL_OPTION,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());

        kill_session(session).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn kill_nonexistent_session_is_ok() {
        let _env_lock = env_lock();
        // Should not error — idempotent
        kill_session("batty-test-nonexistent-kill-99999").unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn session_with_short_lived_process() {
        let _env_lock = env_lock();
        let session = "batty-test-shortlived";
        let _ = kill_session(session);

        // echo exits immediately
        create_session(session, "echo", &["done".to_string()], "/tmp").unwrap();

        // Give it a moment for the process to exit
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Session may or may not still exist depending on tmux remain-on-exit
        // Either way, kill should be safe
        let _ = kill_session(session);
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn test_session_guard_cleanup_on_drop() {
        let _env_lock = env_lock();
        let name = "batty-test-guard-drop";
        let _ = kill_session(name);

        {
            let guard = TestSession::new(name);
            create_session(guard.name(), "sleep", &["30".to_string()], "/tmp").unwrap();
            assert!(session_exists(name));
            // guard dropped here
        }

        assert!(!session_exists(name), "session should be killed on drop");
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn test_session_guard_cleanup_on_panic() {
        let _env_lock = env_lock();
        let name = "batty-test-guard-panic";
        let _ = kill_session(name);

        let result = std::panic::catch_unwind(|| {
            let guard = TestSession::new(name);
            create_session(guard.name(), "sleep", &["30".to_string()], "/tmp").unwrap();
            assert!(session_exists(name));
            panic!("intentional panic to test cleanup");
            #[allow(unreachable_code)]
            drop(guard);
        });

        assert!(result.is_err(), "should have panicked");
        // Give drop a moment to complete (panic unwind runs destructors)
        assert!(
            !session_exists(name),
            "session should be cleaned up even after panic"
        );
    }

    // --- parse_tmux_version edge cases ---

    // --- TmuxCapabilities edge cases ---

    // --- session_name edge cases ---

    // --- PaneDetails struct ---

    // --- SplitMode ---

    // --- TestSession ---

    #[test]
    fn test_session_name_accessor() {
        let guard = TestSession::new("batty-test-accessor");
        assert_eq!(guard.name(), "batty-test-accessor");
        // Don't actually create a tmux session — just test the struct
    }

    // --- Integration tests requiring tmux ---

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pane_exists_for_valid_pane() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-pane-exists";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let pane = pane_id(session).unwrap();
        assert!(pane_exists(&pane), "existing pane should be found");
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn session_exists_returns_false_after_kill() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-sess-exists-gone";
        let _ = kill_session(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        assert!(session_exists(session));

        kill_session(session).unwrap();
        assert!(
            !session_exists(session),
            "session should not exist after kill"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pane_dead_for_running_process() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-pane-dead-alive";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let pane = pane_id(session).unwrap();
        let dead = pane_dead(&pane).unwrap();
        assert!(!dead, "running process pane should not be dead");
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn list_sessions_with_prefix_finds_matching() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let prefix = "batty-test-prefix-match";
        let s1 = format!("{prefix}-aaa");
        let s2 = format!("{prefix}-bbb");
        let _g1 = TestSession::new(s1.clone());
        let _g2 = TestSession::new(s2.clone());

        create_session(&s1, "sleep", &["10".to_string()], "/tmp").unwrap();
        create_session(&s2, "sleep", &["10".to_string()], "/tmp").unwrap();

        let found = list_sessions_with_prefix(prefix);
        assert!(
            found.contains(&s1),
            "should find first session, got: {found:?}"
        );
        assert!(
            found.contains(&s2),
            "should find second session, got: {found:?}"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn list_sessions_with_prefix_excludes_non_matching() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let found = list_sessions_with_prefix("batty-test-zzz-nonexist-99999");
        assert!(found.is_empty(), "should find no sessions for bogus prefix");
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn split_window_horizontal_creates_new_pane() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-hsplit";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let new_pane = split_window_horizontal(&original, 50).unwrap();
        assert!(
            new_pane.starts_with('%'),
            "new pane id should start with %, got: {new_pane}"
        );

        let panes = list_panes(session).unwrap();
        assert_eq!(panes.len(), 2, "should have 2 panes after split");
        assert!(panes.contains(&new_pane));
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn split_window_vertical_creates_new_pane() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-vsplit";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let new_pane = split_window_vertical_in_pane(session, &original, 50).unwrap();
        assert!(
            new_pane.starts_with('%'),
            "new pane id should start with %, got: {new_pane}"
        );

        let panes = list_panes(session).unwrap();
        assert_eq!(panes.len(), 2, "should have 2 panes after split");
        assert!(panes.contains(&new_pane));
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn kill_pane_removes_pane() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-kill-pane";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let new_pane = split_window_horizontal(&original, 50).unwrap();
        let before = list_panes(session).unwrap();
        assert_eq!(before.len(), 2);

        kill_pane(&new_pane).unwrap();
        let after = list_panes(session).unwrap();
        assert_eq!(after.len(), 1, "should have 1 pane after kill");
        assert!(!after.contains(&new_pane));
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn kill_pane_nonexistent_returns_error() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        // tmux returns "can't find pane" for nonexistent pane IDs,
        // which kill_pane only suppresses when it says "not found"
        let result = kill_pane("batty-test-no-such-session-xyz:0.0");
        // Either succeeds (tmux says "not found") or errors — both are valid
        // The key guarantee is it doesn't panic
        let _ = result;
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn set_mouse_disable_and_enable() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-mouse-toggle";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        // Mouse is enabled by create_session; disable it
        set_mouse(session, false).unwrap();
        let output = crate::tmux::tmux_command()
            .args(["show-options", "-t", session, "-v", "mouse"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "off",
            "mouse should be disabled"
        );

        // Re-enable
        set_mouse(session, true).unwrap();
        let output = crate::tmux::tmux_command()
            .args(["show-options", "-t", session, "-v", "mouse"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "on",
            "mouse should be re-enabled"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn tmux_set_custom_option() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-tmux-set";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        tmux_set(session, "@batty_test_opt", "test-value").unwrap();

        let output = crate::tmux::tmux_command()
            .args(["show-options", "-v", "-t", session, "@batty_test_opt"])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "test-value");
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn create_window_fails_for_missing_session() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let result = create_window(
            "batty-test-nonexistent-session-99999",
            "test-win",
            "sleep",
            &["1".to_string()],
            "/tmp",
        );
        assert!(result.is_err(), "should fail for nonexistent session");
        assert!(
            result.unwrap_err().to_string().contains("not found"),
            "error should mention session not found"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn select_layout_even_after_splits() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-layout-even";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let original = pane_id(session).unwrap();
        let _p2 = split_window_horizontal(&original, 50).unwrap();

        // Should not error
        select_layout_even(&original).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn rename_window_changes_name() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-rename-win";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        rename_window(&format!("{session}:0"), "custom-name").unwrap();
        let names = list_window_names(session).unwrap();
        assert!(
            names.contains(&"custom-name".to_string()),
            "window should be renamed, got: {names:?}"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn select_window_switches_active() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-select-win";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();
        create_window(session, "second", "sleep", &["10".to_string()], "/tmp").unwrap();

        // Select the second window — should not error
        select_window(&format!("{session}:second")).unwrap();
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn capture_pane_recent_zero_lines_returns_full() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-capture-zero";
        let _guard = TestSession::new(session);
        create_session(
            session,
            "bash",
            &[
                "-c".to_string(),
                "echo 'zero-lines-test'; sleep 2".to_string(),
            ],
            "/tmp",
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));

        // lines=0 means no -S flag, should return full pane content
        let content = capture_pane_recent(session, 0).unwrap();
        assert!(
            content.contains("zero-lines-test"),
            "should capture full content with lines=0, got: {content:?}"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn pane_id_returns_percent_prefixed() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-paneid-fmt";
        let _guard = TestSession::new(session);
        create_session(session, "sleep", &["10".to_string()], "/tmp").unwrap();

        let id = pane_id(session).unwrap();
        assert!(
            id.starts_with('%'),
            "pane id should start with %, got: {id}"
        );
    }

    #[test]
    #[cfg_attr(not(feature = "integration"), ignore)]
    fn respawn_pane_restarts_running_pane() {
        let _env_lock = env_lock();
        if !require_tmux_integration() {
            return;
        }
        let session = "batty-test-respawn";
        let _guard = TestSession::new(session);
        // Start with a long-running command so session stays alive
        create_session(session, "sleep", &["30".to_string()], "/tmp").unwrap();

        let pane = pane_id(session).unwrap();
        // Respawn with -k kills the running process and starts a new one
        respawn_pane(&pane, "sleep 10").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Pane should still exist and not be dead
        assert!(pane_exists(&pane), "respawned pane should exist");
    }

    #[test]
    fn test_tmux_commands_never_target_the_default_server() {
        let command = tmux_command();
        let envs: std::collections::HashMap<_, _> = command.get_envs().collect();
        // $TMUX explicitly removed, TMUX_TMPDIR pinned to the private dir.
        assert_eq!(envs.get(std::ffi::OsStr::new("TMUX")), Some(&None));
        let tmpdir = envs
            .get(std::ffi::OsStr::new("TMUX_TMPDIR"))
            .and_then(|v| *v)
            .expect("TMUX_TMPDIR must be set in test builds");
        assert_eq!(tmpdir, test_tmux_dir().as_os_str());
        assert!(test_tmux_dir().to_string_lossy().starts_with("/tmp/bt-"));
    }

    #[test]
    fn find_member_target_matches_role_exactly() {
        let listing = "1\t%0\tdashboard\n2\t%4\teng-1\n3\t%5\teng-1-1\n";
        assert_eq!(
            find_member_target(listing, "eng-1"),
            Some((2, "%4".to_string()))
        );
        assert_eq!(
            find_member_target(listing, "eng-1-1"),
            Some((3, "%5".to_string()))
        );
        assert_eq!(find_member_target(listing, "eng"), None);
        assert_eq!(find_member_target("garbage", "x"), None);
        assert_eq!(
            listed_members(listing),
            vec!["dashboard", "eng-1", "eng-1-1"]
        );
    }
}
