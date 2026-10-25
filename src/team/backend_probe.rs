//! Explicit, bounded backend smoke checks. Ordinary validation never invokes these.
use super::{
    agent_options::{self, AgentOptions},
    config::{ClaudeAuthMode, TeamConfig},
    team_config_path,
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn command(options: &AgentOptions) -> Result<(String, Vec<String>)> {
    let mut args: Vec<String>;
    let program = match options.backend.as_deref() {
        Some("claude" | "claude-code") => {
            args = [
                "-p",
                "--output-format",
                "json",
                "--tools",
                "",
                "--disable-slash-commands",
                "--no-session-persistence",
            ]
            .into_iter()
            .map(str::to_string)
            .collect();
            if let Some(model) = &options.model {
                args.extend(["--model".into(), model.clone()]);
            }
            if let Some(effort) = &options.effort {
                args.extend(["--effort".into(), effort.clone()]);
            }
            args.push("Reply only OK.".into());
            "claude"
        }
        Some("codex" | "codex-cli") => {
            args = vec![
                "exec".into(),
                "--json".into(),
                "--ephemeral".into(),
                "--sandbox".into(),
                "read-only".into(),
                "-c".into(),
                "approval_policy=\"never\"".into(),
            ];
            if let Some(model) = &options.model {
                args.extend(["--model".into(), model.clone()]);
            }
            if let Some(effort) = &options.effort {
                args.extend([
                    "-c".into(),
                    format!("model_reasoning_effort={}", serde_json::to_string(effort)?),
                ]);
            }
            args.push("-".into());
            "codex"
        }
        _ => bail!("this backend has no explicit inference probe adapter"),
    };
    Ok((program.into(), args))
}

#[derive(Debug, Serialize)]
pub struct ProbeResult {
    pub checked_at: i64,
    pub requested: AgentOptions,
    pub resolved_model: Option<String>,
    pub succeeded: bool,
}

pub fn run(root: &Path, member: &str, timeout: Duration) -> Result<ProbeResult> {
    if timeout.is_zero() || timeout > Duration::from_secs(120) {
        bail!("probe timeout must be between 1 and 120 seconds");
    }
    let config = TeamConfig::load(&team_config_path(root))?;
    let options = agent_options::all(&config)?
        .into_iter()
        .find(|o| o.member == member)
        .context("unknown member")?;
    let (program, args) = command(&options)?;
    let mut output = tempfile::tempfile()?;
    let errors = tempfile::tempfile()?;
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(output.try_clone()?)
        .stderr(errors);
    let members = super::hierarchy::resolve_hierarchy(&config)?;
    let resolved = members
        .iter()
        .find(|m| m.name == member)
        .context("unknown member")?;
    let role = config
        .role_def(&resolved.role_name)
        .context("unknown role")?;
    if matches!(options.backend.as_deref(), Some("claude" | "claude-code"))
        && config.resolve_claude_auth(role).mode == ClaudeAuthMode::Oauth
    {
        cmd.env_remove("ANTHROPIC_API_KEY");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().context("could not launch backend probe")?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"Reply only OK.\n");
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("backend probe timed out; no raw provider output retained");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        bail!("backend probe failed; check backend authentication and configured model");
    }
    output.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    output.take(64 * 1024).read_to_string(&mut text)?;
    let mut resolved_model = None;
    let mut completed = false;
    for record in text
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    {
        if record.get("is_error").and_then(|v| v.as_bool()) == Some(true)
            || record.get("type").and_then(|v| v.as_str()) == Some("turn.failed")
        {
            bail!("backend probe returned a failure");
        }
        completed |= record.get("result").and_then(|v| v.as_str()).is_some()
            || record.get("type").and_then(|v| v.as_str()) == Some("turn.completed");
        if let Some(model) = record.get("model").and_then(|v| v.as_str()) {
            resolved_model = Some(model.to_string());
        }
        if let Some(models) = record.get("modelUsage").and_then(|v| v.as_object()) {
            resolved_model = models.keys().next().cloned().or(resolved_model);
        }
    }
    if !completed {
        bail!("backend probe did not return a completed response");
    }
    let result = ProbeResult {
        checked_at: chrono::Utc::now().timestamp(),
        requested: options,
        resolved_model,
        succeeded: true,
    };
    let directory = root.join(".batty/probes");
    std::fs::create_dir_all(&directory)?;
    let temporary = directory.join(format!("{member}.{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(&result)?)?;
    std::fs::rename(temporary, directory.join(format!("{member}.json")))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(backend: &str) -> AgentOptions {
        AgentOptions {
            member: "worker-1".into(),
            backend: Some(backend.into()),
            model: None,
            effort: None,
            idle_inference_enabled: true,
            on_demand: false,
        }
    }
    #[test]
    fn probe_is_explicit_and_does_not_invent_overrides() {
        let (_, args) = command(&options("claude")).unwrap();
        assert!(!args.contains(&"--model".into()));
        assert!(!args.contains(&"--effort".into()));
        assert!(args.contains(&"--no-session-persistence".into()));
        assert!(command(&options("unimplemented")).is_err());
    }
    #[test]
    fn codex_effort_is_data_not_config_injection() {
        let mut value = options("codex");
        value.effort = Some("high\"\\injected".into());
        let (_, args) = command(&value).unwrap();
        assert!(
            args.iter()
                .any(|a| a.starts_with("model_reasoning_effort=\"high\\\""))
        );
    }
    #[test]
    fn codex_probe_is_ephemeral_and_read_only() {
        let (_, args) = command(&options("codex")).unwrap();
        assert!(args.iter().any(|arg| arg == "--ephemeral"));
        assert!(
            args.windows(2)
                .any(|args| args == ["--sandbox", "read-only"])
        );
        assert!(args.iter().any(|arg| arg == "approval_policy=\"never\""));
    }
}
