//! Hidden commands that Batty launches for itself (daemon, shim, panes).

use crate::team;
use anyhow::Result;
use std::path::PathBuf;

/// Arguments for the hidden `shim` command.
pub(super) struct ShimOptions {
    pub id: String,
    pub agent_type: String,
    pub cmd: String,
    pub cwd: String,
    pub rows: u16,
    pub cols: u16,
    pub pty_log_path: Option<String>,
    pub graceful_shutdown_timeout_secs: u64,
    pub auto_commit_on_restart: bool,
    pub sdk_mode: bool,
}

pub(super) fn run_watchdog(project_root: String, resume: bool) -> Result<()> {
    team::run_watchdog(std::path::Path::new(&project_root), resume)?;
    Ok(())
}

pub(super) fn run_chat(
    agent_type: String,
    cmd: Option<String>,
    cwd: String,
    sdk_mode: bool,
) -> Result<()> {
    use crate::shim;

    let at: shim::classifier::AgentType =
        agent_type.parse().map_err(|e: String| anyhow::anyhow!(e))?;

    let cmd = if sdk_mode && cmd.is_none() {
        // In SDK mode, select the right protocol per agent type
        match at {
            shim::classifier::AgentType::Kiro => {
                use crate::agent::kiro::KiroCliAdapter;
                KiroCliAdapter::new(None).sdk_launch_command()
            }
            _ => {
                use crate::agent::claude::ClaudeCodeAdapter;
                ClaudeCodeAdapter::new(None).sdk_launch_command(None, None)
            }
        }
    } else {
        cmd.unwrap_or_else(|| shim::chat::default_cmd(at).to_string())
    };

    let cwd = PathBuf::from(&cwd)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&cwd));

    shim::chat::run(at, &cmd, &cwd, sdk_mode)?;
    Ok(())
}

pub(super) fn run_shim(opts: ShimOptions) -> Result<()> {
    let ShimOptions {
        id,
        agent_type,
        cmd,
        cwd,
        rows,
        cols,
        pty_log_path,
        graceful_shutdown_timeout_secs,
        auto_commit_on_restart,
        sdk_mode,
    } = opts;
    use crate::shim;
    use std::os::unix::io::FromRawFd;
    use std::os::unix::net::UnixStream;

    let at: shim::classifier::AgentType =
        agent_type.parse().map_err(|e: String| anyhow::anyhow!(e))?;

    // Recover the channel socket from fd 3 (inherited from parent).
    let stream = unsafe { UnixStream::from_raw_fd(3) };
    let channel = shim::protocol::Channel::new(stream);

    let args = shim::runtime::ShimArgs {
        id,
        agent_type: at,
        cmd,
        cwd: PathBuf::from(cwd),
        rows,
        cols,
        pty_log_path: pty_log_path.map(PathBuf::from),
        graceful_shutdown_timeout_secs,
        auto_commit_on_restart,
    };

    if sdk_mode {
        match at {
            shim::classifier::AgentType::Codex => {
                shim::runtime_codex::run_codex_sdk(args, channel)?;
            }
            shim::classifier::AgentType::Kiro => {
                shim::runtime_kiro::run_kiro_acp(args, channel)?;
            }
            _ => {
                shim::runtime_sdk::run_sdk(args, channel)?;
            }
        }
    } else {
        shim::runtime::run(args, channel)?;
    }
    Ok(())
}

pub(super) fn run_console_pane(
    project_root: String,
    member: String,
    events_log_path: String,
    pty_log_path: String,
) -> Result<()> {
    crate::console_pane::run(
        PathBuf::from(project_root).as_path(),
        &member,
        PathBuf::from(events_log_path).as_path(),
        PathBuf::from(pty_log_path).as_path(),
    )?;
    Ok(())
}

pub(super) fn run_daemon(project_root: String, resume: bool) -> Result<()> {
    let root = std::path::PathBuf::from(project_root);
    team::run_daemon(&root, resume)?;
    Ok(())
}
