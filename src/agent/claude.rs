//! Claude Code adapter.

use anyhow::Context;
use uuid::Uuid;

use crate::agent::AgentAdapter;

/// Adapter for Claude Code CLI.
pub struct ClaudeCodeAdapter {
    /// Override the claude binary name/path (default: "claude").
    program: String,
}

impl ClaudeCodeAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "claude".to_string()),
        }
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn launch_command(
        &self,
        prompt: &str,
        idle: bool,
        resume: bool,
        session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let escaped = prompt.replace('\'', "'\\''");
        if resume {
            let sid = session_id.context("missing Claude session ID for resume")?;
            Ok(format!(
                "exec claude --dangerously-skip-permissions --resume '{sid}'"
            ))
        } else if idle {
            let session_flag = session_id
                .map(|id| format!(" --session-id '{id}'"))
                .unwrap_or_default();
            Ok(format!(
                "exec claude --dangerously-skip-permissions{session_flag} --append-system-prompt '{escaped}'"
            ))
        } else {
            let session_flag = session_id
                .map(|id| format!(" --session-id '{id}'"))
                .unwrap_or_default();
            Ok(format!(
                "exec claude --dangerously-skip-permissions{session_flag} '{escaped}'"
            ))
        }
    }

    fn new_session_id(&self) -> Option<String> {
        Some(Uuid::new_v4().to_string())
    }

    fn health_check(&self) -> super::BackendHealth {
        super::check_binary_available(&self.program)
    }
}

impl ClaudeCodeAdapter {
    /// Build the launch command for SDK (stream-json) mode.
    ///
    /// Returns a shell command that starts Claude Code in non-interactive mode
    /// with structured NDJSON I/O on stdin/stdout.
    ///
    /// `system_prompt` is the role prompt injected via `--append-system-prompt`.
    /// Pass `None` to omit it (e.g., for `batty chat` interactive mode).
    pub fn sdk_launch_command(
        &self,
        session_id: Option<&str>,
        system_prompt: Option<&str>,
    ) -> String {
        let mut cmd = format!(
            "exec {} -p --verbose --input-format=stream-json --output-format=stream-json --permission-mode=bypassPermissions",
            self.program,
        );
        if let Some(sid) = session_id {
            let escaped = sid.replace('\'', "'\\''");
            cmd.push_str(&format!(" --session-id '{escaped}'"));
        }
        if let Some(prompt) = system_prompt {
            let escaped = prompt.replace('\'', "'\\''");
            cmd.push_str(&format!(" --append-system-prompt '{escaped}'"));
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_claude_code() {
        let adapter = ClaudeCodeAdapter::new(None);
        assert_eq!(adapter.name(), "claude-code");
    }

    // --- Backend trait method tests ---

    #[test]
    fn launch_command_active_includes_prompt() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter
            .launch_command("do the thing", false, false, Some("sess-1"))
            .unwrap();
        assert!(cmd.contains("exec claude --dangerously-skip-permissions"));
        assert!(cmd.contains("--session-id 'sess-1'"));
        assert!(cmd.contains("'do the thing'"));
        assert!(!cmd.contains("--append-system-prompt"));
    }

    #[test]
    fn launch_command_idle_uses_append_system_prompt() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter
            .launch_command("role prompt", true, false, Some("sess-2"))
            .unwrap();
        assert!(cmd.contains("--append-system-prompt"));
        assert!(cmd.contains("--session-id 'sess-2'"));
    }

    #[test]
    fn launch_command_resume_uses_resume_flag() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", false, true, Some("sess-3"))
            .unwrap();
        assert!(cmd.contains("--resume 'sess-3'"));
        assert!(!cmd.contains("--append-system-prompt"));
    }

    #[test]
    fn launch_command_resume_without_session_id_errors() {
        let adapter = ClaudeCodeAdapter::new(None);
        let result = adapter.launch_command("ignored", false, true, None);
        assert!(result.is_err());
    }

    #[test]
    fn launch_command_escapes_single_quotes() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter
            .launch_command("fix user's bug", false, false, None)
            .unwrap();
        assert!(cmd.contains("user'\\''s"));
    }

    #[test]
    fn new_session_id_returns_uuid() {
        let adapter = ClaudeCodeAdapter::new(None);
        let session_id = adapter.new_session_id();
        assert!(session_id.is_some());
        let sid = session_id.unwrap();
        assert_eq!(sid.len(), 36); // UUID v4 format
    }

    // --- SDK launch command tests ---

    #[test]
    fn sdk_launch_command_includes_stream_json_flags() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter.sdk_launch_command(None, None);
        assert!(cmd.contains("exec claude"));
        assert!(cmd.contains("-p"));
        assert!(cmd.contains("--verbose"));
        assert!(cmd.contains("--input-format=stream-json"));
        assert!(cmd.contains("--output-format=stream-json"));
        assert!(cmd.contains("--permission-mode=bypassPermissions"));
        assert!(!cmd.contains("--session-id"));
        assert!(!cmd.contains("--append-system-prompt"));
    }

    #[test]
    fn sdk_launch_command_with_session_id() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter.sdk_launch_command(Some("sess-abc-123"), None);
        assert!(cmd.contains("--session-id 'sess-abc-123'"));
    }

    #[test]
    fn sdk_launch_command_with_system_prompt() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter.sdk_launch_command(None, Some("You are an engineer."));
        assert!(cmd.contains("--append-system-prompt 'You are an engineer.'"));
    }

    #[test]
    fn sdk_launch_command_escapes_prompt_quotes() {
        let adapter = ClaudeCodeAdapter::new(None);
        let cmd = adapter.sdk_launch_command(None, Some("Fix user's bug"));
        assert!(cmd.contains("user'\\''s"));
    }

    #[test]
    fn sdk_launch_command_custom_binary() {
        let adapter = ClaudeCodeAdapter::new(Some("/opt/claude".to_string()));
        let cmd = adapter.sdk_launch_command(None, None);
        assert!(cmd.contains("exec /opt/claude -p"));
    }
}
