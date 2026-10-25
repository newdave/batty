//! Read-only launch validation and explicit operator recovery, with no inference.
use super::{agent_options, config::TeamConfig, team_config_path};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Report {
    pub team: String,
    pub executable_sha256: String,
    pub version: &'static str,
    pub members: Vec<agent_options::AgentOptions>,
    pub verification_command: Option<String>,
    pub diagnostics: Vec<String>,
}

pub fn inspect(root: &Path, runtime: bool) -> Result<Report> {
    let config = TeamConfig::load(&team_config_path(root))?;
    config.validate()?;
    config.validate_project_refs(root)?;
    let members = agent_options::all(&config)?;
    let mut diagnostics = Vec::new();
    for member in &members {
        if root
            .join(".batty/backend-configuration")
            .join(format!("{}.json", member.member))
            .is_file()
        {
            diagnostics.push(format!(
                "{}: backend configuration requires repair",
                member.member
            ));
        }
    }
    for role in config.board.tag_roles.values() {
        if config.role_def(role).is_none() {
            diagnostics.push(format!("tag route refers to unknown role '{role}'"));
        }
    }
    let explicit_command = config
        .workflow_policy
        .verification
        .test_command
        .as_deref()
        .or(config.workflow_policy.test_command.as_deref());
    let verification_command = if config.workflow_policy.verification.auto_run_tests {
        match super::task_loop::resolve_test_command(root, explicit_command) {
            Ok(command) => Some(command),
            Err(error) => {
                diagnostics.push(error.to_string());
                None
            }
        }
    } else {
        explicit_command.map(str::to_string)
    };
    if runtime {
        diagnostics.extend(runtime_diagnostics(root, &members)?);
    }
    let executable_sha256 = format!(
        "{:x}",
        Sha256::digest(std::fs::read(std::env::current_exe()?)?)
    );
    Ok(Report {
        team: config.name,
        executable_sha256,
        version: env!("CARGO_PKG_VERSION"),
        members,
        verification_command,
        diagnostics,
    })
}

pub(crate) fn runtime_diagnostics(
    root: &Path,
    members: &[agent_options::AgentOptions],
) -> Result<Vec<String>> {
    let mut diagnostics = Vec::new();
    let path = root.join(".batty/launch-state.json");
    let identities: HashMap<String, serde_json::Value> = serde_json::from_slice(
        &std::fs::read(path).context("runtime identity state unavailable")?,
    )?;
    let mut seen = HashSet::new();
    for member in members {
        let Some(identity) = identities.get(&member.member) else {
            diagnostics.push(format!("{}: missing launch identity", member.member));
            continue;
        };
        let backend = identity.get("agent").and_then(|v| v.as_str()).unwrap_or("");
        let expected = match member.backend.as_deref() {
            Some("claude") => "claude-code",
            Some("codex") => "codex-cli",
            Some("kiro") => "kiro-cli",
            Some(s) => s,
            None => "",
        };
        if backend != expected {
            diagnostics.push(format!(
                "{}: backend differs from current configuration",
                member.member
            ));
        }
        if let Some(sid) = identity.get("session_id").and_then(|v| v.as_str()) {
            if !seen.insert((backend.to_string(), sid.to_string())) {
                diagnostics.push(format!("{}: duplicate session identity", member.member));
            }
        } else if expected == "claude-code" {
            diagnostics.push(format!("{}: missing session ID", member.member));
        }
        let record_path = root
            .join(".batty/launch-options")
            .join(format!("{}.json", member.member));
        let record: Option<agent_options::AgentOptions> = std::fs::read(record_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if record.as_ref().is_none_or(|r| {
            r.model != member.model
                || r.effort != member.effort
                || r.backend != member.backend
                || r.idle_inference_enabled != member.idle_inference_enabled
        }) {
            diagnostics.push(format!(
                "{}: launch settings missing or changed since launch",
                member.member
            ));
        }
    }
    Ok(diagnostics)
}

pub fn reset_backend(root: &Path, member: &str) -> Result<()> {
    let config = TeamConfig::load(&team_config_path(root))?;
    if !agent_options::all(&config)?
        .iter()
        .any(|m| m.member == member)
    {
        bail!("unknown member '{member}'");
    }
    let path = root
        .join(".batty/backend-configuration")
        .join(format!("{member}.json"));
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_sessions_and_changed_options_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".batty/launch-options")).unwrap();
        let config: TeamConfig = serde_yaml::from_str("name: sample\nroles:\n- name: worker\n  role_type: engineer\n  agent: claude\n  instances: 2\n").unwrap();
        let members = agent_options::all(&config).unwrap();
        let mut ids = serde_json::Map::new();
        for member in &members {
            ids.insert(
                member.member.clone(),
                serde_json::json!({"agent":"claude-code","session_id":"same-session"}),
            );
            std::fs::write(
                root.join(".batty/launch-options")
                    .join(format!("{}.json", member.member)),
                serde_json::to_vec(member).unwrap(),
            )
            .unwrap();
        }
        std::fs::write(
            root.join(".batty/launch-state.json"),
            serde_json::to_vec(&ids).unwrap(),
        )
        .unwrap();
        let diagnostics = runtime_diagnostics(root, &members).unwrap();
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("duplicate session identity"));
        let mut changed = members.clone();
        changed[0].model = Some("another-model".into());
        assert!(
            runtime_diagnostics(root, &changed)
                .unwrap()
                .iter()
                .any(|s| s.contains("changed since launch"))
        );
    }
    #[test]
    fn reset_is_scoped_to_a_configured_member() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".batty/team_config")).unwrap();
        std::fs::create_dir_all(root.join(".batty/backend-configuration")).unwrap();
        std::fs::write(
            team_config_path(root),
            "name: sample\nroles:\n- name: worker\n  role_type: engineer\n  agent: claude\n",
        )
        .unwrap();
        let marker = root.join(".batty/backend-configuration/worker.json");
        std::fs::write(&marker, "{}").unwrap();
        assert!(reset_backend(root, "../worker").is_err());
        assert!(marker.exists());
        reset_backend(root, "worker").unwrap();
        assert!(!marker.exists());
        reset_backend(root, "worker").unwrap();
    }
}
