//! Per-member Codex SDK launch context. Authentication stays in the user's Codex home.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchContext {
    pub role_context: String,
    pub resume_session_id: Option<String>,
}

fn path(cwd: &Path, member: &str) -> Result<PathBuf> {
    ensure!(
        !member.is_empty()
            && member != "."
            && member != ".."
            && !member.contains(['/', '\\', '\0', '\n', '\r']),
        "invalid Codex member name"
    );
    Ok(cwd
        .join(".batty/codex-context")
        .join(member)
        .join("launch.json"))
}

impl LaunchContext {
    pub fn load(cwd: &Path, member: &str) -> Result<Self> {
        let bytes = match std::fs::read(path(cwd, member)?) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        let context: Self =
            serde_json::from_slice(&bytes).context("invalid Codex launch context")?;
        if let Some(id) = &context.resume_session_id {
            uuid::Uuid::parse_str(id).context("invalid Codex resume thread ID")?;
        }
        Ok(context)
    }

    pub fn save(&self, cwd: &Path, member: &str) -> Result<()> {
        let target = path(cwd, member)?;
        std::fs::create_dir_all(target.parent().context("missing Codex context parent")?)?;
        let temporary = target.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec(self)?)?;
        std::fs::rename(temporary, target)?;
        Ok(())
    }

    pub fn prompt(&self, first_turn: bool, message: &str) -> String {
        if first_turn && !self.role_context.is_empty() {
            format!(
                "Batty role instructions:\n{}\n\nAssigned message:\n{message}",
                self.role_context
            )
        } else {
            message.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_thread_and_role_context_survive_restart_without_repeating_role_each_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let context = LaunchContext {
            role_context: "Review independently".into(),
            resume_session_id: Some(uuid::Uuid::new_v4().to_string()),
        };
        context.save(tmp.path(), "quality-1").unwrap();
        let loaded = LaunchContext::load(tmp.path(), "quality-1").unwrap();
        assert_eq!(loaded.resume_session_id, context.resume_session_id);
        assert!(
            loaded
                .prompt(true, "Check tests")
                .contains("Review independently")
        );
        assert_eq!(loaded.prompt(false, "Check tests"), "Check tests");
        assert!(LaunchContext::load(tmp.path(), "../quality-1").is_err());
    }
}
