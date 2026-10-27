//! Codex CLI adapter.
//!
//! Runs Codex in interactive mode by default, passing the composed task prompt
//! as the initial user prompt argument.

use anyhow::Context;

use crate::agent::AgentAdapter;

/// Adapter for Codex CLI.
pub struct CodexCliAdapter {
    /// Override the codex binary name/path (default: "codex").
    program: String,
}

impl CodexCliAdapter {
    pub fn new(program: Option<String>) -> Self {
        Self {
            program: program.unwrap_or_else(|| "codex".to_string()),
        }
    }
}

impl AgentAdapter for CodexCliAdapter {
    fn name(&self) -> &str {
        "codex-cli"
    }

    fn launch_command(
        &self,
        prompt: &str,
        idle: bool,
        resume: bool,
        session_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let escaped = prompt.replace('\'', "'\\''");
        let prefix = format!(
            "{} --dangerously-bypass-approvals-and-sandbox",
            self.program
        );
        if resume {
            let sid = session_id.context("missing Codex session ID for resume")?;
            let fallback = if idle {
                format!("exec {prefix}")
            } else {
                format!("exec {prefix} '{escaped}'")
            };
            Ok(format!(
                "{program} resume '{sid}' --dangerously-bypass-approvals-and-sandbox || {fallback}",
                program = self.program,
            ))
        } else if idle {
            Ok(format!("exec {prefix}"))
        } else {
            Ok(format!("{prefix} '{escaped}'"))
        }
    }

    fn health_check(&self) -> super::BackendHealth {
        super::check_binary_available(&self.program)
    }
}

impl CodexCliAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_codex_cli() {
        let adapter = CodexCliAdapter::new(None);
        assert_eq!(adapter.name(), "codex-cli");
    }

    // --- Backend trait method tests ---

    #[test]
    fn launch_command_active_includes_prompt() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("do the thing", false, false, None)
            .unwrap();
        assert!(cmd.contains("codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("'do the thing'"));
        // Active (non-resume) should NOT use exec so the shim can detect exit
        assert!(!cmd.starts_with("exec "));
    }

    #[test]
    fn launch_command_idle_omits_prompt() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", true, false, None)
            .unwrap();
        assert_eq!(cmd, "exec codex --dangerously-bypass-approvals-and-sandbox");
    }

    #[test]
    fn launch_command_resume_uses_session_id() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", false, true, Some("codex-sess-1"))
            .unwrap();
        assert!(cmd.contains("codex resume 'codex-sess-1'"));
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("|| exec codex --dangerously-bypass-approvals-and-sandbox 'ignored'"));
    }

    #[test]
    fn launch_command_resume_idle_falls_back_to_fresh_idle_start() {
        let adapter = CodexCliAdapter::new(None);
        let cmd = adapter
            .launch_command("ignored", true, true, Some("codex-sess-1"))
            .unwrap();
        assert!(cmd.contains("codex resume 'codex-sess-1'"));
        assert!(cmd.contains("|| exec codex --dangerously-bypass-approvals-and-sandbox"));
        assert!(!cmd.contains("'ignored'"));
    }

    #[test]
    fn launch_command_resume_without_session_id_errors() {
        let adapter = CodexCliAdapter::new(None);
        let result = adapter.launch_command("ignored", false, true, None);
        assert!(result.is_err());
    }

    #[test]
    fn new_session_id_returns_none() {
        let adapter = CodexCliAdapter::new(None);
        assert!(adapter.new_session_id().is_none());
    }
}
