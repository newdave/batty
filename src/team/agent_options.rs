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

/// Effort override recorded on the task the member currently holds (set by
/// bench recovery). Only tasks that are in progress or in review count.
pub(crate) fn active_task_effort_override(root: &Path, member: &str) -> Option<String> {
    let tasks_dir = super::team_config_dir(root).join("board").join("tasks");
    crate::task::load_tasks_from_dir(&tasks_dir)
        .ok()?
        .into_iter()
        .filter(|task| {
            task.claimed_by.as_deref() == Some(member)
                && matches!(task.status.as_str(), "in-progress" | "review")
        })
        .find_map(|task| {
            super::bench_recovery::read_bench_recovery(&task.source_path).effort_override
        })
}

/// Replace the configured effort with the active task's override. Backends
/// without effort support keep their configuration: the override is skipped
/// with a debug log, never an error.
pub(crate) fn apply_task_effort_override(root: &Path, options: &mut AgentOptions) {
    let Some(effort) = active_task_effort_override(root, &options.member) else {
        return;
    };
    let supported = options
        .backend
        .as_deref()
        .is_some_and(super::init::backend_supports_effort);
    if !supported {
        tracing::debug!(
            member = %options.member,
            backend = ?options.backend,
            "skipping task effort override: backend does not support effort"
        );
        return;
    }
    if validate_value("effort", Some(&effort)).is_ok() {
        options.effort = Some(effort);
    }
}

/// The effort a member must be relaunched with for its active task, or
/// `None` when no relaunch is needed: bench recovery is off, the task has no
/// usable override, the override equals the configured effort, no launch has
/// been recorded, or the recorded launch already used the effective effort.
pub(crate) fn task_effort_relaunch_needed(root: &Path, member: &str) -> Option<String> {
    let config = TeamConfig::load(&team_config_path(root)).ok()?;
    if !config.workflow_policy.bench_recovery.enabled {
        return None;
    }
    let configured = all(&config)
        .ok()?
        .into_iter()
        .find(|o| o.member == member)?;
    let mut effective = configured.clone();
    apply_task_effort_override(root, &mut effective);
    if effective.effort == configured.effort {
        return None;
    }
    let record: AgentOptions = std::fs::read(
        root.join(".batty/launch-options")
            .join(format!("{member}.json")),
    )
    .ok()
    .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
    if record.effort == effective.effort {
        return None;
    }
    effective.effort
}

pub fn resolve(root: &Path, member: &str) -> Result<AgentOptions> {
    let path = team_config_path(root);
    if path.is_file() {
        let config = TeamConfig::load(&path)?;
        if let Some(mut options) = all(&config)?.into_iter().find(|o| o.member == member) {
            if config.workflow_policy.bench_recovery.enabled {
                apply_task_effort_override(root, &mut options);
            }
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
    fn write_override_project(root: &Path, agent: &str, enabled: bool) {
        let team_dir = root.join(".batty/team_config");
        std::fs::create_dir_all(team_dir.join("board/tasks")).unwrap();
        std::fs::write(
            team_dir.join("team.yaml"),
            format!(
                "name: sample\nagent: {agent}\nworkflow_policy:\n  bench_recovery:\n    enabled: {enabled}\nroles:\n- name: worker\n  role_type: engineer\n  effort: medium\n"
            ),
        )
        .unwrap();
        std::fs::write(
            team_dir.join("board/tasks/007-task.md"),
            "---\nid: 7\ntitle: t\nstatus: in-progress\npriority: high\nclaimed_by: worker\neffort_override: high\n---\n\nbody\n",
        )
        .unwrap();
    }

    fn record_launch(root: &Path, member: &str, effort: &str) {
        let mut options = resolve(root, member).unwrap();
        options.effort = Some(effort.to_string());
        let dir = root.join(".batty/launch-options");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{member}.json")),
            serde_json::to_vec(&options).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn resolve_applies_active_task_effort_override() {
        let tmp = tempfile::tempdir().unwrap();
        write_override_project(tmp.path(), "claude", true);
        let options = resolve(tmp.path(), "worker").unwrap();
        assert_eq!(options.effort.as_deref(), Some("high"));
    }

    #[test]
    fn resolve_ignores_override_when_bench_recovery_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        write_override_project(tmp.path(), "claude", false);
        let options = resolve(tmp.path(), "worker").unwrap();
        assert_eq!(options.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn resolve_ignores_override_for_other_members_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        write_override_project(tmp.path(), "claude", true);
        let task = tmp
            .path()
            .join(".batty/team_config/board/tasks/007-task.md");
        let content = std::fs::read_to_string(&task).unwrap();
        std::fs::write(
            &task,
            content.replace("claimed_by: worker", "claimed_by: other"),
        )
        .unwrap();
        assert_eq!(
            resolve(tmp.path(), "worker").unwrap().effort.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn override_is_skipped_for_backend_without_effort_support() {
        let tmp = tempfile::tempdir().unwrap();
        write_override_project(tmp.path(), "kiro", true);
        let team = tmp.path().join(".batty/team_config/team.yaml");
        let content = std::fs::read_to_string(&team).unwrap();
        std::fs::write(&team, content.replace("  effort: medium\n", "")).unwrap();
        let options = resolve(tmp.path(), "worker").unwrap();
        assert_eq!(options.effort, None);
    }

    #[test]
    fn relaunch_needed_only_when_launch_effort_differs() {
        let tmp = tempfile::tempdir().unwrap();
        write_override_project(tmp.path(), "claude", true);
        record_launch(tmp.path(), "worker", "medium");
        assert_eq!(
            task_effort_relaunch_needed(tmp.path(), "worker").as_deref(),
            Some("high")
        );
        record_launch(tmp.path(), "worker", "high");
        assert_eq!(task_effort_relaunch_needed(tmp.path(), "worker"), None);
    }

    #[test]
    fn relaunch_not_needed_without_override_or_launch_record() {
        let tmp = tempfile::tempdir().unwrap();
        write_override_project(tmp.path(), "claude", true);
        assert_eq!(task_effort_relaunch_needed(tmp.path(), "worker"), None);
        record_launch(tmp.path(), "worker", "medium");
        std::fs::remove_file(
            tmp.path()
                .join(".batty/team_config/board/tasks/007-task.md"),
        )
        .unwrap();
        assert_eq!(task_effort_relaunch_needed(tmp.path(), "worker"), None);
    }

    #[test]
    fn rejects_empty_or_multiline_and_quotes_shell_data() {
        assert!(validate_value("model", Some(" ")).is_err());
        assert!(validate_value("effort", Some("high\nexit")).is_err());
        assert_eq!(shell_quote("user's model"), "'user'\\''s model'");
    }
}
