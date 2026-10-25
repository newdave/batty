//! Backend-neutral launch settings resolved independently for each member.

use super::{
    config::{RoleType, TeamConfig},
    hierarchy, team_config_path,
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOptions {
    pub member: String,
    pub backend: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub idle_inference_enabled: bool,
    pub on_demand: bool,
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn validate_value(name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        if value.trim().is_empty() || value.contains(['\0', '\n', '\r']) {
            bail!("{name} must be a nonempty single-line value");
        }
    }
    Ok(())
}

pub fn all(config: &TeamConfig) -> Result<Vec<AgentOptions>> {
    hierarchy::resolve_hierarchy(config)?
        .into_iter()
        .filter(|m| m.role_type != RoleType::User)
        .map(|m| {
            if m.name.is_empty()
                || m.name == "."
                || m.name == ".."
                || m.name.contains(['/', '\\', '\0', '\n', '\r'])
            {
                bail!("member names must be single safe path components");
            }
            let role = config.role_def(&m.role_name).expect("resolved role exists");
            let effort = role
                .instance_overrides
                .get(&m.name)
                .and_then(|o| o.effort.clone())
                .or_else(|| role.effort.clone());
            validate_value("model", m.model.as_deref())?;
            validate_value("effort", effort.as_deref())?;
            if effort.is_some()
                && !matches!(
                    m.agent.as_deref(),
                    Some("claude" | "claude-code" | "codex" | "codex-cli")
                )
            {
                bail!(
                    "member '{}' backend does not support effort overrides",
                    m.name
                );
            }
            Ok(AgentOptions {
                member: m.name,
                backend: m.agent,
                model: m.model,
                effort,
                on_demand: role.on_demand,
                idle_inference_enabled: config
                    .workflow_policy
                    .idle_inference_enabled
                    .unwrap_or(!role.on_demand),
            })
        })
        .collect()
}

pub fn resolve(root: &Path, member: &str) -> Result<AgentOptions> {
    let path = team_config_path(root);
    if path.is_file() {
        if let Some(options) = all(&TeamConfig::load(&path)?)?
            .into_iter()
            .find(|o| o.member == member)
        {
            return Ok(options);
        }
    }
    Ok(AgentOptions {
        member: member.into(),
        backend: None,
        model: None,
        effort: None,
        idle_inference_enabled: true,
        on_demand: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(extra: &str) -> TeamConfig {
        serde_yaml::from_str(&format!("name: sample\nroles:\n- name: reviewer\n  role_type: engineer\n  agent: claude\n{extra}")).unwrap()
    }
    #[test]
    fn omitted_settings_do_not_invent_model_or_effort() {
        let values = all(&config("")).unwrap();
        assert!(values[0].model.is_none() && values[0].effort.is_none());
        assert!(!values[0].idle_inference_enabled);
    }
    #[test]
    fn demand_and_instance_effort_are_configurable() {
        let values = all(&config("  on_demand: true\n  effort: low\n  model: custom-model\n  instance_overrides:\n    reviewer:\n      effort: high")).unwrap();
        assert_eq!(values[0].effort.as_deref(), Some("high"));
        assert!(!values[0].idle_inference_enabled);
        assert_eq!(values[0].model.as_deref(), Some("custom-model"));
    }
    #[test]
    fn rejects_empty_or_multiline_and_quotes_shell_data() {
        assert!(validate_value("model", Some(" ")).is_err());
        assert!(validate_value("effort", Some("high\nexit")).is_err());
        assert_eq!(shell_quote("user's model"), "'user'\\''s model'");
    }
}
