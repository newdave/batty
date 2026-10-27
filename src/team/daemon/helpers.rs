//! Free helper functions used by the daemon module.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::agent::BackendHealth;
use crate::team::board_cmd;
use crate::team::config::RoleType;
use crate::team::git_cmd;
use crate::team::hierarchy::MemberInstance;
use crate::tmux;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MemberWorktreeContext {
    pub path: PathBuf,
    pub branch: Option<String>,
}

pub(super) fn describe_command_failure(
    command: &str,
    args: &[&str],
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    let details = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("process exited with status {status}")
    };

    format!("`{command} {}` failed: {details}", args.join(" "))
}

pub(super) fn default_prompt_file_for_role(role_type: RoleType) -> &'static str {
    match role_type {
        RoleType::Architect => "architect.md",
        RoleType::Manager => "manager.md",
        RoleType::Engineer => "engineer.md",
        RoleType::User => "architect.md",
    }
}

pub(super) fn role_prompt_path(
    team_config_dir: &Path,
    prompt_override: Option<&str>,
    role_type: RoleType,
) -> PathBuf {
    team_config_dir.join(prompt_override.unwrap_or(default_prompt_file_for_role(role_type)))
}

/// Extract the `## Nudge` section from a prompt .md file.
///
/// Returns the text after `## Nudge` up to the next `## ` heading or EOF.
/// Returns `None` if no `## Nudge` section is found.
pub(super) fn extract_nudge_section(prompt_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(prompt_path).ok()?;
    let mut in_nudge = false;
    let mut lines = Vec::new();

    for line in content.lines() {
        if line.starts_with("## Nudge") {
            in_nudge = true;
            continue;
        }
        if in_nudge {
            // Stop at next heading
            if line.starts_with("## ") {
                break;
            }
            lines.push(line);
        }
    }

    if lines.is_empty() {
        return None;
    }

    let text = lines.join("\n").trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

pub(super) fn format_stuck_duration(stuck_age_secs: u64) -> String {
    if stuck_age_secs >= 3600 {
        let hours = stuck_age_secs / 3600;
        let mins = (stuck_age_secs % 3600) / 60;
        format!("{hours}h {mins}m")
    } else if stuck_age_secs >= 60 {
        let mins = stuck_age_secs / 60;
        let secs = stuck_age_secs % 60;
        format!("{mins}m {secs}s")
    } else {
        format!("{stuck_age_secs}s")
    }
}

pub(super) fn ensure_tmux_session_ready(session: &str) -> Result<()> {
    if tmux::session_exists(session) {
        Ok(())
    } else {
        bail!("daemon startup pre-flight failed: tmux session '{session}' is missing")
    }
}

pub(super) fn ensure_kanban_available() -> Result<()> {
    let output = Command::new("kanban-md")
        .arg("--help")
        .output()
        .context(
            "daemon startup pre-flight failed while verifying board tooling: could not execute `kanban-md --help`",
        )?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        "unknown error".to_string()
    } else {
        stderr
    };
    bail!("daemon startup pre-flight failed: `kanban-md --help` failed: {detail}");
}

pub(super) fn board_dir(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("board")
}

pub(super) fn ensure_board_initialized(project_root: &Path) -> Result<bool> {
    let board_dir = board_dir(project_root);
    if board_dir.join("tasks").is_dir() {
        return Ok(false);
    }

    board_cmd::init(&board_dir).map_err(|error| {
        anyhow::anyhow!(
            "daemon startup pre-flight failed: unable to initialize board at '{}': {error}",
            board_dir.display()
        )
    })?;
    Ok(true)
}

pub(super) fn ensure_git_ready(project_root: &Path) -> Result<()> {
    let repo_check = crate::git::cmd(project_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .allow_fail()
        .run()
        .context(
            "daemon startup pre-flight failed while verifying git repository: could not execute `git rev-parse --is-inside-work-tree`",
        )?;
    if !repo_check.status.success() || String::from_utf8_lossy(&repo_check.stdout).trim() != "true"
    {
        bail!("daemon startup pre-flight failed: project root is not a git repository");
    }

    let dirty_lines: Vec<String> = git_cmd::status_porcelain(project_root)
        .context("daemon startup pre-flight failed while checking git dirty state")?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| {
            let path = line.get(3..).unwrap_or("").trim();
            let path = path.split(" -> ").last().unwrap_or(path).trim_matches('"');
            !path.starts_with(".batty/")
        })
        .map(str::to_string)
        .collect();
    if !dirty_lines.is_empty() {
        // Consistent with the running daemon: warn (auto-merges are blocked)
        // rather than refusing to start while a human edits files in the root.
        tracing::warn!(
            dirty = %dirty_lines.join("\n"),
            "project root has dirty source changes that will block review auto-merges"
        );
    }

    for key in ["user.name", "user.email"] {
        let output = crate::git::cmd(project_root)
        .args(["config", "--get", key])
        .allow_fail()
        .run()
            .with_context(|| {
                format!(
                    "daemon startup pre-flight failed while verifying git config: could not execute `git config --get {key}`"
                )
            })?;
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            bail!("daemon startup pre-flight failed: git config '{key}' is missing");
        }
    }

    Ok(())
}

pub(super) fn ensure_worktree_operations(project_root: &Path) -> Result<()> {
    let probe_root = project_root.join(".batty").join("startup-preflight");
    std::fs::create_dir_all(&probe_root).with_context(|| {
        format!(
            "daemon startup pre-flight failed: unable to create startup preflight directory '{}'",
            probe_root.display()
        )
    })?;
    let worktree_dir = probe_root.join(format!(
        "worktree-probe-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let worktree_path = worktree_dir.to_string_lossy().to_string();

    let add_output = crate::git::cmd(project_root)
        .args(["worktree", "add", "--detach", &worktree_path, "HEAD"])
        .allow_fail()
        .run()
        .context(
            "daemon startup pre-flight failed while verifying git worktree support: could not execute `git worktree add`",
        )?;
    if !add_output.status.success() {
        let detail = describe_command_failure(
            "git",
            &["worktree", "add", "--detach", &worktree_path, "HEAD"],
            add_output.status,
            &add_output.stdout,
            &add_output.stderr,
        );
        bail!("daemon startup pre-flight failed: {detail}");
    }

    let remove_output = crate::git::cmd(project_root)
        .args(["worktree", "remove", "--force", &worktree_path])
        .allow_fail()
        .run()
        .context(
            "daemon startup pre-flight failed while verifying git worktree cleanup: could not execute `git worktree remove`",
        )?;
    if !remove_output.status.success() {
        let detail = describe_command_failure(
            "git",
            &["worktree", "remove", "--force", &worktree_path],
            remove_output.status,
            &remove_output.stdout,
            &remove_output.stderr,
        );
        let _ = std::fs::remove_dir_all(&worktree_dir);
        bail!("daemon startup pre-flight failed: {detail}");
    }

    if worktree_dir.exists() {
        let _ = std::fs::remove_dir_all(&worktree_dir);
        bail!(
            "daemon startup pre-flight failed: temporary worktree '{}' was not removed",
            worktree_dir.display()
        );
    }

    Ok(())
}

pub(super) fn ensure_telemetry_writable(project_root: &Path) -> Result<()> {
    let conn = crate::team::telemetry_db::open(project_root)
        .context("daemon startup pre-flight failed: telemetry.db is not writable")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS __batty_preflight_probe (id INTEGER)",
        [],
    )
    .context("daemon startup pre-flight failed: telemetry.db probe write failed")?;
    Ok(())
}

pub(super) fn ensure_agent_binaries_available(members: &[MemberInstance]) -> Result<()> {
    for member in members {
        if member.role_type == RoleType::User {
            continue;
        }

        let agent_name = member.agent.as_deref().unwrap_or("claude");
        let Some(health) = crate::agent::health_check_by_name(agent_name) else {
            bail!(
                "daemon startup pre-flight failed: unknown agent backend '{}' for member '{}'",
                agent_name,
                member.name
            );
        };
        if health != BackendHealth::Healthy {
            bail!(
                "daemon startup pre-flight failed: backend '{}' for member '{}' is {}",
                agent_name,
                member.name,
                health.as_str()
            );
        }
    }

    Ok(())
}

/// Convert a recorded child pid to a `pid_t` that is safe to signal.
///
/// `kill(0, ..)` signals the caller's process group and `kill(-1, ..)` every
/// process the caller may signal, so zero and values that wrap negative are
/// rejected.
pub(super) fn signalable_pid(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// Whether `pid` names a live process. Zombies still count as alive; use
/// [`reap_if_exited`] for children of this process.
pub(super) fn pid_alive(pid: u32) -> bool {
    let Some(pid) = signalable_pid(pid) else {
        return false;
    };
    // SAFETY: signal 0 only performs an existence/permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Send `signal` to `pid`, ignoring invalid pids. Returns true if delivered.
pub(super) fn kill_pid(pid: u32, signal: libc::c_int) -> bool {
    let Some(pid) = signalable_pid(pid) else {
        return false;
    };
    // SAFETY: pid is validated positive.
    unsafe { libc::kill(pid, signal) == 0 }
}

/// Non-blocking exit check for a child of this process. Reaps the child when
/// it has exited (so it does not linger as a zombie). For pids that are not
/// our children, falls back to an existence check.
pub(super) fn reap_if_exited(pid: u32) -> bool {
    let Some(raw) = signalable_pid(pid) else {
        return true;
    };
    let mut status = 0;
    // SAFETY: raw is validated positive and status is a valid out pointer.
    let rc = unsafe { libc::waitpid(raw, &mut status, libc::WNOHANG) };
    match rc {
        0 => false,
        r if r == raw => true,
        _ => !pid_alive(pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn signalable_pid_rejects_zero_and_wrapping_values() {
        assert_eq!(signalable_pid(0), None);
        assert_eq!(signalable_pid(u32::MAX), None);
        assert_eq!(signalable_pid(i32::MAX as u32 + 1), None);
        assert_eq!(signalable_pid(42), Some(42));
    }

    #[test]
    fn invalid_pids_are_never_signalled() {
        assert!(!pid_alive(0));
        assert!(!kill_pid(0, libc::SIGKILL));
        assert!(reap_if_exited(0));
    }

    #[test]
    fn reap_if_exited_reaps_zombie_children() {
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !reap_if_exited(pid) {
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The zombie was reaped (std sees ECHILD), so the pid no longer exists.
        let _ = child.try_wait();
        assert!(!pid_alive(pid));
    }

    #[test]
    fn reap_if_exited_is_false_for_running_child() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(pid_alive(pid));
        assert!(!reap_if_exited(pid));
        assert!(kill_pid(pid, libc::SIGKILL));
        child.wait().unwrap();
    }
}
