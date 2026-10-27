//! Team initialization, template management, and run export.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::{
    TEAM_CONFIG_FILE, daemon_log_path, now_unix, orchestrator_log_path, team_config_dir,
    team_config_path, team_events_path,
};

fn canonical_backend(agent: &str) -> String {
    crate::agent::adapter_from_name(agent)
        .map(|adapter| adapter.name().to_string())
        .unwrap_or_else(|| agent.into())
}

fn backend_supports_effort(agent: &str) -> bool {
    matches!(
        canonical_backend(agent).as_str(),
        "claude-code" | "codex-cli"
    )
}

/// Claude model pins are aliases (`sonnet`, `opus`, `haiku`) or `claude-*` ids;
/// they only make sense on a Claude backend. Any other model name is assumed
/// valid for every non-Claude backend.
fn model_valid_for_backend(model: &str, agent: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let claude_model = model.starts_with("claude")
        || ["sonnet", "opus", "haiku"]
            .iter()
            .any(|alias| model.starts_with(alias));
    claude_model == (canonical_backend(agent) == "claude-code")
}

/// Leading-space count of `line`.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// Split `key: value  # comment` into `(key, value, comment)` where `comment`
/// keeps its leading whitespace and `#`.
fn split_key_line(line: &str) -> Option<(&str, &str, &str)> {
    let body = line.trim_start_matches(' ');
    let body = body.strip_prefix("- ").map(str::trim_start).unwrap_or(body);
    let (key, rest) = body.split_once(':')?;
    if key.is_empty() || key.contains([' ', '#', '"', '\'']) {
        return None;
    }
    let (value, comment) = match rest.find(" #") {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, ""),
    };
    Some((key, value.trim(), comment))
}

#[derive(Default)]
struct MappingLines {
    /// Line index of the `agent:`, `model:` and `effort:` keys, if present.
    agent: Option<usize>,
    model: Option<usize>,
    effort: Option<usize>,
    /// Index into the group list of the enclosing mapping.
    parent: Option<usize>,
}

/// Point the roles of `yaml` at `agent` with targeted line edits, so comments,
/// ordering and quoting elsewhere in the template survive.
///
/// `model` and `effort` pins are kept unless they are invalid for the new
/// backend (a Claude model alias on another backend, or an effort on a backend
/// that has no effort control). Returns the new text and one human-readable
/// note per role whose pins changed.
fn override_template_backend_with_notes(yaml: &str, agent: &str) -> Result<(String, Vec<String>)> {
    use serde_yaml::Value;

    let had_trailing_newline = yaml.ends_with('\n');
    let mut lines: Vec<Option<String>> = yaml.lines().map(|l| Some(l.to_string())).collect();
    let agent_scalar = serde_yaml::to_string(agent).context("failed to serialize agent name")?;
    let agent_scalar = agent_scalar.trim_end().to_string();

    let Some(roles_at) = lines.iter().position(|l| l.as_deref() == Some("roles:")) else {
        return Ok((yaml.to_string(), Vec::new()));
    };
    let roles_end = lines[roles_at + 1..]
        .iter()
        .position(|l| {
            l.as_deref()
                .is_some_and(|l| !l.trim().is_empty() && !l.starts_with([' ', '#', '-']))
        })
        .map_or(lines.len(), |p| roles_at + 1 + p);

    // Role items start at the smallest indent carrying a `- ` marker.
    let item_indent = lines[roles_at + 1..roles_end]
        .iter()
        .flatten()
        .filter(|l| l.trim_start().starts_with("- "))
        .map(|l| indent_of(l))
        .min();
    let Some(item_indent) = item_indent else {
        return Ok((yaml.to_string(), Vec::new()));
    };
    let mut starts: Vec<usize> = (roles_at + 1..roles_end)
        .filter(|&i| {
            lines[i]
                .as_deref()
                .is_some_and(|l| indent_of(l) == item_indent && l.trim_start().starts_with("- "))
        })
        .collect();
    starts.push(roles_end);

    let mut notes = Vec::new();
    let mut inserts: Vec<(usize, String)> = Vec::new();
    for pair in starts.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        let first = lines[start].clone().unwrap_or_default();
        if first.contains('{') && first.trim_start().starts_with("- {") {
            bail!("flow-style role at line {} is not supported", start + 1);
        }
        let key_indent = item_indent + 2;

        // Collect mappings (the role itself and nested instance overrides).
        let mut role_name = String::new();
        let mut is_user = false;
        let mut groups: Vec<MappingLines> = vec![MappingLines::default()];
        // (indent, group index) of the mappings currently open.
        let mut stack: Vec<(usize, usize)> = vec![(key_indent, 0)];
        let mut pending_child = false;
        for (i, line) in lines.iter().enumerate().take(end).skip(start) {
            let Some(line) = line.as_deref() else {
                continue;
            };
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let indent = if i == start {
                key_indent
            } else {
                indent_of(line)
            };
            while stack.last().is_some_and(|&(ind, _)| ind > indent) {
                stack.pop();
            }
            if pending_child && stack.last().is_none_or(|&(ind, _)| indent > ind) {
                let parent = stack.last().map(|&(_, g)| g);
                groups.push(MappingLines {
                    parent,
                    ..Default::default()
                });
                stack.push((indent, groups.len() - 1));
            }
            pending_child = false;
            let Some(&(_, group)) = stack.last() else {
                continue;
            };
            let Some((key, value, _)) = split_key_line(line) else {
                continue;
            };
            if group == 0 {
                match key {
                    "name" => role_name = value.trim_matches(['"', '\'']).to_string(),
                    "role_type" => is_user = value == "user",
                    _ => {}
                }
            }
            match key {
                "agent" => groups[group].agent = Some(i),
                "model" => groups[group].model = Some(i),
                "effort" => groups[group].effort = Some(i),
                _ => {}
            }
            if value.is_empty() {
                pending_child = true;
            }
        }
        if is_user {
            continue;
        }

        let mut changed_flags: Vec<bool> = Vec::with_capacity(groups.len());
        let mut role_notes = Vec::new();
        for (idx, group) in groups.iter().enumerate() {
            let inherited = group.parent.map(|p| changed_flags[p]).unwrap_or(true);
            let old_agent = group.agent.and_then(|i| {
                split_key_line(lines[i].as_deref()?)
                    .map(|(_, v, _)| v.trim_matches(['"', '\'']).to_string())
            });
            let changed = old_agent
                .as_deref()
                .map(|old| canonical_backend(old) != canonical_backend(agent))
                .unwrap_or(inherited);
            changed_flags.push(changed);

            if let Some(i) = group.agent {
                let old = lines[i].clone().unwrap_or_default();
                if let Some((_, _, comment)) = split_key_line(&old) {
                    lines[i] = Some(format!(
                        "{}agent: {agent_scalar}{comment}",
                        " ".repeat(indent_of(&old))
                    ));
                }
            } else if idx == 0 {
                inserts.push((
                    start + 1,
                    format!("{}agent: {agent_scalar}", " ".repeat(key_indent)),
                ));
            }
            if !changed {
                continue;
            }
            for (slot, kind) in [(group.model, "model"), (group.effort, "effort")] {
                let Some(i) = slot else { continue };
                let Some((_, value, _)) = lines[i].as_deref().and_then(split_key_line) else {
                    continue;
                };
                let value = value.trim_matches(['"', '\'']).to_string();
                let valid = if kind == "model" {
                    model_valid_for_backend(&value, agent)
                } else {
                    backend_supports_effort(agent)
                };
                if valid {
                    role_notes.push(format!("kept {kind} '{value}'"));
                } else {
                    lines[i] = None;
                    role_notes.push(format!("dropped {kind} '{value}' (not valid for {agent})"));
                }
            }
        }
        if !role_notes.is_empty() {
            notes.push(format!("role '{role_name}': {}", role_notes.join(", ")));
        }
    }

    let mut out: Vec<String> = Vec::with_capacity(lines.len() + inserts.len());
    for (i, line) in lines.into_iter().enumerate() {
        for (at, text) in &inserts {
            if *at == i {
                out.push(text.clone());
            }
        }
        if let Some(line) = line {
            out.push(line);
        }
    }
    let mut updated = out.join("\n");
    if had_trailing_newline {
        updated.push('\n');
    }

    // Safety net: every non-user role must now name the requested backend.
    let parsed: Value = serde_yaml::from_str(&updated)
        .context("backend override produced invalid team config YAML")?;
    if let Some(roles) = parsed.get("roles").and_then(Value::as_sequence) {
        for role in roles {
            let is_user = role.get("role_type").and_then(Value::as_str) == Some("user");
            let backend = role.get("agent").and_then(Value::as_str);
            if !is_user && backend.map(canonical_backend) != Some(canonical_backend(agent)) {
                bail!("backend override did not apply to every role; check the template layout");
            }
        }
    }
    Ok((updated, notes))
}

#[cfg(test)]
fn override_template_backend(yaml: &str, agent: &str) -> Result<String> {
    override_template_backend_with_notes(yaml, agent).map(|(yaml, _)| yaml)
}

/// Returns `~/.batty/templates/`.
pub fn templates_base_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("cannot determine home directory")?;
    Ok(PathBuf::from(home).join(".batty").join("templates"))
}

/// Overrides collected from the interactive init wizard.
#[derive(Debug, Default)]
pub struct InitOverrides {
    pub orchestrator_pane: Option<bool>,
    pub auto_dispatch: Option<bool>,
    pub use_worktrees: Option<bool>,
    pub timeout_nudges: Option<bool>,
    pub standups: Option<bool>,
    pub triage_interventions: Option<bool>,
    pub review_interventions: Option<bool>,
    pub owned_task_interventions: Option<bool>,
    pub manager_dispatch_interventions: Option<bool>,
    pub architect_utilization_interventions: Option<bool>,
    pub auto_merge_enabled: Option<bool>,
    pub standup_interval_secs: Option<u64>,
    pub nudge_interval_secs: Option<u64>,
    pub stall_threshold_secs: Option<u64>,
    pub review_nudge_threshold_secs: Option<u64>,
    pub review_timeout_secs: Option<u64>,
}

/// Scaffold `.batty/team_config/` with default team.yaml and prompt templates.
pub fn init_team(
    project_root: &Path,
    template: &str,
    project_name: Option<&str>,
    agent: Option<&str>,
    force: bool,
) -> Result<Vec<PathBuf>> {
    init_team_with_overrides(project_root, template, project_name, agent, force, None)
}

/// Scaffold with optional interactive overrides applied to the template YAML.
pub fn init_team_with_overrides(
    project_root: &Path,
    template: &str,
    project_name: Option<&str>,
    agent: Option<&str>,
    force: bool,
    overrides: Option<&InitOverrides>,
) -> Result<Vec<PathBuf>> {
    let config_dir = team_config_dir(project_root);
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;

    let mut created = Vec::new();

    let yaml_path = config_dir.join(TEAM_CONFIG_FILE);
    if yaml_path.exists() && !force {
        bail!(
            "team config already exists at {}; remove it first or use --force",
            yaml_path.display()
        );
    }

    let yaml_content = match template {
        "solo" => include_str!("templates/team_solo.yaml"),
        "pair" => include_str!("templates/team_pair.yaml"),
        "squad" => include_str!("templates/team_squad.yaml"),
        "large" => include_str!("templates/team_large.yaml"),
        "research" => include_str!("templates/team_research.yaml"),
        "software" => include_str!("templates/team_software.yaml"),
        "cleanroom" => include_str!("templates/team_cleanroom.yaml"),
        "batty" => include_str!("templates/team_batty.yaml"),
        "python" => include_str!("templates/team_python.yaml"),
        "mixed" => include_str!("templates/team_mixed.yaml"),
        _ => include_str!("templates/team_simple.yaml"),
    };
    let mut yaml_content = yaml_content.to_string();
    if let Some(name) = project_name {
        yaml_content = set_template_name(&yaml_content, name)?;
    }
    if let Some(agent_name) = agent {
        let (updated, notes) = override_template_backend_with_notes(&yaml_content, agent_name)?;
        yaml_content = updated;
        for note in notes {
            println!("note: --agent {agent_name}: {note}");
        }
    }
    if let Some(ov) = overrides {
        yaml_content = apply_init_overrides(&yaml_content, ov);
    }
    crate::atomic_file::write(&yaml_path, &yaml_content)?;
    created.push(yaml_path);

    // Install prompt .md files matching the template's roles
    let prompt_files: &[(&str, &str)] = match template {
        "research" => &[
            (
                "research_lead.md",
                include_str!("templates/research_lead.md"),
            ),
            ("sub_lead.md", include_str!("templates/sub_lead.md")),
            ("researcher.md", include_str!("templates/researcher.md")),
        ],
        "software" => &[
            ("tech_lead.md", include_str!("templates/tech_lead.md")),
            ("eng_manager.md", include_str!("templates/eng_manager.md")),
            ("developer.md", include_str!("templates/developer.md")),
        ],
        "batty" => &[
            (
                "batty_architect.md",
                include_str!("templates/batty_architect.md"),
            ),
            (
                "batty_manager.md",
                include_str!("templates/batty_manager.md"),
            ),
            (
                "batty_engineer.md",
                include_str!("templates/batty_engineer.md"),
            ),
        ],
        "cleanroom" => &[
            (
                "batty_decompiler.md",
                include_str!("templates/batty_decompiler.md"),
            ),
            (
                "batty_spec_writer.md",
                include_str!("templates/batty_spec_writer.md"),
            ),
            (
                "batty_test_writer.md",
                include_str!("templates/batty_test_writer.md"),
            ),
            (
                "batty_implementer.md",
                include_str!("templates/batty_implementer.md"),
            ),
        ],
        "python" => &[
            ("architect.md", include_str!("templates/architect.md")),
            ("manager.md", include_str!("templates/manager.md")),
            (
                "python_engineer.md",
                include_str!("templates/python_engineer.md"),
            ),
        ],
        "simple" | "mixed" => &[
            ("architect.md", include_str!("templates/architect.md")),
            ("manager.md", include_str!("templates/manager.md")),
            ("engineer.md", include_str!("templates/engineer.md")),
            (
                "research_specialist.md",
                include_str!("templates/research_specialist.md"),
            ),
            (
                "documentation_specialist.md",
                include_str!("templates/documentation_specialist.md"),
            ),
            (
                "quality_specialist.md",
                include_str!("templates/quality_specialist.md"),
            ),
        ],
        _ => &[
            ("architect.md", include_str!("templates/architect.md")),
            ("manager.md", include_str!("templates/manager.md")),
            ("engineer.md", include_str!("templates/engineer.md")),
        ],
    };

    for (name, content) in prompt_files {
        let path = config_dir.join(name);
        if force || !path.exists() {
            crate::atomic_file::write(&path, content)?;
            created.push(path);
        }
    }

    let directive_files = [
        (
            "replenishment_context.md",
            include_str!("templates/replenishment_context.md"),
        ),
        (
            "review_policy.md",
            include_str!("templates/review_policy.md"),
        ),
        (
            "escalation_policy.md",
            include_str!("templates/escalation_policy.md"),
        ),
    ];
    for (name, content) in directive_files {
        let path = config_dir.join(name);
        if force || !path.exists() {
            crate::atomic_file::write(&path, content)?;
            created.push(path);
        }
    }

    if template == "cleanroom" {
        scaffold_cleanroom_assets(project_root, force, &mut created)?;
    }

    // Initialize kanban-md board in the team config directory
    let board_dir = config_dir.join("board");
    if !board_dir.exists() {
        let output = std::process::Command::new("kanban-md")
            .args(["init", "--dir", &board_dir.to_string_lossy()])
            .output();
        match output {
            Ok(out) if out.status.success() => {
                created.push(board_dir);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn_kanban_md_unavailable(&format!("`kanban-md init` failed: {}", stderr.trim()));
                let kanban_path = config_dir.join("kanban.md");
                std::fs::write(
                    &kanban_path,
                    "# Kanban Board\n\n## Backlog\n\n## In Progress\n\n## Done\n",
                )?;
                created.push(kanban_path);
            }
            Err(_) => {
                warn_kanban_md_unavailable("`kanban-md` was not found on PATH");
                let kanban_path = config_dir.join("kanban.md");
                std::fs::write(
                    &kanban_path,
                    "# Kanban Board\n\n## Backlog\n\n## In Progress\n\n## Done\n",
                )?;
                created.push(kanban_path);
            }
        }
    }

    info!(dir = %config_dir.display(), files = created.len(), "scaffolded team config");
    Ok(created)
}

/// Loudly tell the user the board could not be initialised: `batty start` and
/// `batty board` hard-require kanban-md, so the plain kanban.md fallback alone
/// does not make the project runnable.
fn warn_kanban_md_unavailable(reason: &str) {
    warn!("{reason}; falling back to plain kanban.md");
    eprintln!("warning: {reason}; the task board was NOT initialised.");
    eprintln!(
        "warning: `batty start` and `batty board` require kanban-md. Install it \
         (e.g. `go install github.com/antopolskiy/kanban-md/cmd/kanban-md@latest`, \
         see https://github.com/antopolskiy/kanban-md), then run `kanban-md init --dir \
         .batty/team_config/board`."
    );
}

/// Replace the leading `name:` line of a template with `name`, serialised as a
/// YAML scalar so `:`, `#` or leading `-[{` cannot produce invalid YAML.
fn set_template_name(yaml: &str, name: &str) -> Result<String> {
    if name.trim().is_empty() {
        bail!("project name must not be empty");
    }
    if name.chars().any(char::is_control) {
        bail!("project name must not contain control characters");
    }
    let scalar = serde_yaml::to_string(name).context("failed to serialize project name")?;
    let scalar = scalar.trim_end();
    let Some(end) = yaml.find('\n') else {
        return Ok(yaml.to_string());
    };
    let updated = format!("name: {scalar}{}", &yaml[end..]);
    let parsed: serde_yaml::Value = serde_yaml::from_str(&updated)
        .with_context(|| format!("project name {name:?} produced invalid team config YAML"))?;
    if parsed.get("name").and_then(serde_yaml::Value::as_str) != Some(name) {
        bail!("project name {name:?} did not round-trip through team config YAML");
    }
    Ok(updated)
}

fn scaffold_cleanroom_assets(
    project_root: &Path,
    force: bool,
    created: &mut Vec<PathBuf>,
) -> Result<()> {
    let analysis_dir = project_root.join("analysis");
    let implementation_dir = project_root.join("implementation");
    let planning_dir = project_root.join("planning");
    let specs_dir = project_root.join("specs");
    let verification_scripts_dir = project_root.join("scripts").join("verification");

    create_dir_if_missing(&analysis_dir, created)?;
    create_dir_if_missing(&implementation_dir, created)?;
    create_dir_if_missing(&planning_dir, created)?;
    create_dir_if_missing(&specs_dir, created)?;
    create_dir_if_missing(&verification_scripts_dir, created)?;

    write_scaffold_file(
        &project_root.join("PARITY.md"),
        include_str!("templates/cleanroom_PARITY.md"),
        force,
        created,
    )?;
    write_scaffold_file(
        &project_root.join("SPEC.md"),
        include_str!("templates/cleanroom_SPEC.md"),
        force,
        created,
    )?;
    write_scaffold_file(
        &planning_dir.join("cleanroom-process.md"),
        include_str!("templates/cleanroom_process.md"),
        force,
        created,
    )?;
    write_scaffold_file(
        &project_root.join(".batty").join("verification.yml"),
        include_str!("templates/cleanroom_verification.yml"),
        force,
        created,
    )?;
    write_scaffold_file(
        &verification_scripts_dir.join("placeholder-baseline.sh"),
        include_str!("templates/cleanroom_verification_baseline.sh"),
        force,
        created,
    )?;
    write_scaffold_file(
        &verification_scripts_dir.join("placeholder-candidate.sh"),
        include_str!("templates/cleanroom_verification_candidate.sh"),
        force,
        created,
    )?;
    mark_executable(
        &verification_scripts_dir.join("placeholder-baseline.sh"),
        force,
    )?;
    mark_executable(
        &verification_scripts_dir.join("placeholder-candidate.sh"),
        force,
    )?;
    write_scaffold_file(
        &analysis_dir.join("README.md"),
        include_str!("templates/cleanroom_analysis.md"),
        force,
        created,
    )?;

    Ok(())
}

fn create_dir_if_missing(path: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        created.push(path.to_path_buf());
    }
    Ok(())
}

fn write_scaffold_file(
    path: &Path,
    content: &str,
    force: bool,
    created: &mut Vec<PathBuf>,
) -> Result<()> {
    if force || !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        crate::atomic_file::write(path, content)?;
        created.push(path.to_path_buf());
    }
    Ok(())
}

fn mark_executable(path: &Path, force: bool) -> Result<()> {
    if force || path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mut perms = std::fs::metadata(path)
                .with_context(|| format!("failed to stat {}", path.display()))?
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms)
                .with_context(|| format!("failed to chmod {}", path.display()))?;
        }
    }
    Ok(())
}

/// Apply interactive overrides to template YAML content via text replacement.
fn apply_init_overrides(yaml: &str, ov: &InitOverrides) -> String {
    let mut doc: serde_yaml::Value = match serde_yaml::from_str(yaml) {
        Ok(v) => v,
        Err(_) => return yaml.to_string(),
    };
    let map = match doc.as_mapping_mut() {
        Some(m) => m,
        None => return yaml.to_string(),
    };

    fn set_bool(map: &mut serde_yaml::Mapping, section: &str, key: &str, val: Option<bool>) {
        if let Some(v) = val {
            let sec = map
                .entry(serde_yaml::Value::String(section.to_string()))
                .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            if let Some(m) = sec.as_mapping_mut() {
                m.insert(
                    serde_yaml::Value::String(key.to_string()),
                    serde_yaml::Value::Bool(v),
                );
            }
        }
    }

    fn set_u64(map: &mut serde_yaml::Mapping, section: &str, key: &str, val: Option<u64>) {
        if let Some(v) = val {
            let sec = map
                .entry(serde_yaml::Value::String(section.to_string()))
                .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            if let Some(m) = sec.as_mapping_mut() {
                m.insert(
                    serde_yaml::Value::String(key.to_string()),
                    serde_yaml::Value::Number(serde_yaml::Number::from(v)),
                );
            }
        }
    }

    if let Some(v) = ov.orchestrator_pane {
        map.insert(
            serde_yaml::Value::String("orchestrator_pane".to_string()),
            serde_yaml::Value::Bool(v),
        );
    }

    set_bool(map, "board", "auto_dispatch", ov.auto_dispatch);
    set_u64(map, "standup", "interval_secs", ov.standup_interval_secs);
    set_bool(map, "automation", "timeout_nudges", ov.timeout_nudges);
    set_bool(map, "automation", "standups", ov.standups);
    set_bool(
        map,
        "automation",
        "triage_interventions",
        ov.triage_interventions,
    );
    set_bool(
        map,
        "automation",
        "review_interventions",
        ov.review_interventions,
    );
    set_bool(
        map,
        "automation",
        "owned_task_interventions",
        ov.owned_task_interventions,
    );
    set_bool(
        map,
        "automation",
        "manager_dispatch_interventions",
        ov.manager_dispatch_interventions,
    );
    set_bool(
        map,
        "automation",
        "architect_utilization_interventions",
        ov.architect_utilization_interventions,
    );
    set_u64(
        map,
        "workflow_policy",
        "stall_threshold_secs",
        ov.stall_threshold_secs,
    );
    set_u64(
        map,
        "workflow_policy",
        "review_nudge_threshold_secs",
        ov.review_nudge_threshold_secs,
    );
    set_u64(
        map,
        "workflow_policy",
        "review_timeout_secs",
        ov.review_timeout_secs,
    );

    if let Some(v) = ov.auto_merge_enabled {
        let sec = map
            .entry(serde_yaml::Value::String("workflow_policy".to_string()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        if let Some(wp) = sec.as_mapping_mut() {
            let am = wp
                .entry(serde_yaml::Value::String("auto_merge".to_string()))
                .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            if let Some(m) = am.as_mapping_mut() {
                m.insert(
                    serde_yaml::Value::String("enabled".to_string()),
                    serde_yaml::Value::Bool(v),
                );
            }
        }
    }

    if ov.use_worktrees.is_some() || ov.nudge_interval_secs.is_some() {
        if let Some(roles) = map
            .get_mut(serde_yaml::Value::String("roles".to_string()))
            .and_then(|v| v.as_sequence_mut())
        {
            for role in roles.iter_mut() {
                if let Some(m) = role.as_mapping_mut() {
                    let role_type = m
                        .get(serde_yaml::Value::String("role_type".to_string()))
                        .and_then(|v| v.as_str())
                        .map(str::to_owned);

                    if role_type.as_deref() == Some("engineer") {
                        if let Some(v) = ov.use_worktrees {
                            m.insert(
                                serde_yaml::Value::String("use_worktrees".to_string()),
                                serde_yaml::Value::Bool(v),
                            );
                        }
                    }
                    if role_type.as_deref() == Some("architect") {
                        if let Some(v) = ov.nudge_interval_secs {
                            m.insert(
                                serde_yaml::Value::String("nudge_interval_secs".to_string()),
                                serde_yaml::Value::Number(serde_yaml::Number::from(v)),
                            );
                        }
                    }
                }
            }
        }
    }

    serde_yaml::to_string(&doc).unwrap_or_else(|_| yaml.to_string())
}

pub fn list_available_templates() -> Result<Vec<String>> {
    let templates_dir = templates_base_dir()?;
    if !templates_dir.is_dir() {
        bail!(
            "no templates directory found at {}",
            templates_dir.display()
        );
    }

    let mut templates = Vec::new();
    for entry in std::fs::read_dir(&templates_dir)
        .with_context(|| format!("failed to read {}", templates_dir.display()))?
    {
        let entry = entry?;
        if entry.path().is_dir() {
            templates.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    templates.sort();
    Ok(templates)
}

const TEMPLATE_PROJECT_ROOT_DIR: &str = "project_root";

fn copy_template_dir(src: &Path, dst: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("failed to create {}", dst.display()))?;
    for entry in
        std::fs::read_dir(src).with_context(|| format!("failed to read {}", src.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_template_dir(&src_path, &dst_path, created)?;
        } else {
            std::fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "failed to copy template file from {} to {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
            created.push(dst_path);
        }
    }
    Ok(())
}

pub fn init_from_template(project_root: &Path, template_name: &str) -> Result<Vec<PathBuf>> {
    let templates_dir = templates_base_dir()?;
    if !templates_dir.is_dir() {
        bail!(
            "no templates directory found at {}",
            templates_dir.display()
        );
    }

    let available = list_available_templates()?;
    if !available.iter().any(|name| name == template_name) {
        let available_display = if available.is_empty() {
            "(none)".to_string()
        } else {
            available.join(", ")
        };
        bail!(
            "template '{}' not found in {}; available templates: {}",
            template_name,
            templates_dir.display(),
            available_display
        );
    }

    let config_dir = team_config_dir(project_root);
    let yaml_path = config_dir.join(TEAM_CONFIG_FILE);
    if yaml_path.exists() {
        bail!(
            "team config already exists at {}; remove it first or edit directly",
            yaml_path.display()
        );
    }

    let source_dir = templates_dir.join(template_name);
    let mut created = Vec::new();
    std::fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;
    for entry in std::fs::read_dir(&source_dir)
        .with_context(|| format!("failed to read {}", source_dir.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        if file_name == TEMPLATE_PROJECT_ROOT_DIR {
            copy_template_dir(&src_path, project_root, &mut created)?;
        } else if src_path.is_dir() {
            copy_template_dir(&src_path, &config_dir.join(file_name), &mut created)?;
        } else {
            let dst_path = config_dir.join(file_name);
            copy_template_file(&src_path, &dst_path)?;
            created.push(dst_path);
        }
    }
    info!(
        template = template_name,
        source = %source_dir.display(),
        dest = %config_dir.display(),
        files = created.len(),
        "copied team config from user template"
    );
    Ok(created)
}

/// Export the current team config as a reusable template.
pub fn export_template(project_root: &Path, name: &str) -> Result<usize> {
    let config_dir = team_config_dir(project_root);
    let team_yaml = config_dir.join(TEAM_CONFIG_FILE);
    if !team_yaml.is_file() {
        bail!("team config missing at {}", team_yaml.display());
    }

    let template_dir = templates_base_dir()?.join(name);
    if template_dir.exists() {
        eprintln!(
            "warning: overwriting existing template at {}",
            template_dir.display()
        );
    }
    std::fs::create_dir_all(&template_dir)
        .with_context(|| format!("failed to create {}", template_dir.display()))?;

    let mut copied = 0usize;
    copy_template_file(&team_yaml, &template_dir.join(TEAM_CONFIG_FILE))?;
    copied += 1;

    let mut prompt_paths = std::fs::read_dir(&config_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect::<Vec<_>>();
    prompt_paths.sort();

    for source in prompt_paths {
        let file_name = source
            .file_name()
            .context("template source missing file name")?;
        copy_template_file(&source, &template_dir.join(file_name))?;
        copied += 1;
    }

    copied += export_project_template_assets(project_root, &template_dir)?;

    Ok(copied)
}

fn export_project_template_assets(project_root: &Path, template_dir: &Path) -> Result<usize> {
    let mut copied = 0usize;
    let export_root = template_dir.join(TEMPLATE_PROJECT_ROOT_DIR);

    let optional_dirs = [
        (
            project_root.join("analysis"),
            export_root.join("analysis"),
            false,
        ),
        (
            project_root.join("implementation"),
            export_root.join("implementation"),
            false,
        ),
        (
            project_root.join("planning"),
            export_root.join("planning"),
            true,
        ),
    ];
    for (source, destination, cleanroom_only) in optional_dirs {
        if cleanroom_only && source.file_name() == Some(std::ffi::OsStr::new("planning")) {
            let cleanroom_doc = source.join("cleanroom-process.md");
            if cleanroom_doc.is_file() {
                copy_template_file(&cleanroom_doc, &destination.join("cleanroom-process.md"))?;
                copied += 1;
            }
            continue;
        }
        if source.is_dir() {
            let mut created = Vec::new();
            copy_template_dir(&source, &destination, &mut created)?;
            copied += count_files_in_dir(&source)?;
        }
    }

    let optional_files = [
        (
            project_root.join("PARITY.md"),
            export_root.join("PARITY.md"),
        ),
        (project_root.join("SPEC.md"), export_root.join("SPEC.md")),
    ];
    for (source, destination) in optional_files {
        if source.is_file() {
            copy_template_file(&source, &destination)?;
            copied += 1;
        }
    }

    Ok(copied)
}

pub fn export_run(project_root: &Path) -> Result<PathBuf> {
    let team_yaml = team_config_path(project_root);
    if !team_yaml.is_file() {
        bail!("team config missing at {}", team_yaml.display());
    }

    let export_dir = create_run_export_dir(project_root)?;
    copy_template_file(&team_yaml, &export_dir.join(TEAM_CONFIG_FILE))?;

    copy_dir_if_exists(
        &team_config_dir(project_root).join("board").join("tasks"),
        &export_dir.join("board").join("tasks"),
    )?;
    copy_file_if_exists(
        &team_events_path(project_root),
        &export_dir.join("events.jsonl"),
    )?;
    copy_file_if_exists(
        &daemon_log_path(project_root),
        &export_dir.join("daemon.log"),
    )?;
    copy_file_if_exists(
        &orchestrator_log_path(project_root),
        &export_dir.join("orchestrator.log"),
    )?;
    copy_dir_if_exists(
        &project_root.join(".batty").join("retrospectives"),
        &export_dir.join("retrospectives"),
    )?;
    copy_file_if_exists(
        &project_root.join(".batty").join("test_timing.jsonl"),
        &export_dir.join("test_timing.jsonl"),
    )?;

    Ok(export_dir)
}

fn copy_template_file(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::copy(source, destination).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(())
}

fn exports_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("exports")
}

fn create_run_export_dir(project_root: &Path) -> Result<PathBuf> {
    let base = exports_dir(project_root);
    std::fs::create_dir_all(&base)
        .with_context(|| format!("failed to create {}", base.display()))?;

    let timestamp = now_unix();
    let primary = base.join(timestamp.to_string());
    if !primary.exists() {
        std::fs::create_dir(&primary)
            .with_context(|| format!("failed to create {}", primary.display()))?;
        return Ok(primary);
    }

    for suffix in 1.. {
        let candidate = base.join(format!("{timestamp}-{suffix}"));
        if candidate.exists() {
            continue;
        }
        std::fs::create_dir(&candidate)
            .with_context(|| format!("failed to create {}", candidate.display()))?;
        return Ok(candidate);
    }

    unreachable!("infinite suffix iterator should always return or continue");
}

fn copy_file_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if source.is_file() {
        copy_template_file(source, destination)?;
    }
    Ok(())
}

fn copy_dir_if_exists(source: &Path, destination: &Path) -> Result<()> {
    if source.is_dir() {
        let mut created = Vec::new();
        copy_template_dir(source, destination, &mut created)?;
    }
    Ok(())
}

fn count_files_in_dir(path: &Path) -> Result<usize> {
    let mut count = 0usize;
    for entry in
        std::fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
    {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            count += count_files_in_dir(&child)?;
        } else {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{EnvVarGuard, env_lock};

    use crate::team::{
        daemon_log_path, orchestrator_log_path, team_config_dir, team_config_path, team_events_path,
    };

    #[test]
    fn default_templates_do_not_enable_auto_merge() {
        for template in ["simple", "mixed"] {
            let tmp = tempfile::tempdir().unwrap();
            init_team(tmp.path(), template, None, None, false).unwrap();
            let config =
                crate::team::config::TeamConfig::load(&team_config_path(tmp.path())).unwrap();
            assert!(
                !config.workflow_policy.auto_merge.enabled,
                "{template} template must not enable auto-merge"
            );
        }
    }

    #[test]
    fn init_team_creates_scaffolding() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "simple", None, None, false).unwrap();
        assert!(!created.is_empty());
        assert!(team_config_path(tmp.path()).exists());
        assert!(team_config_dir(tmp.path()).join("architect.md").exists());
        assert!(team_config_dir(tmp.path()).join("manager.md").exists());
        assert!(team_config_dir(tmp.path()).join("engineer.md").exists());
        assert!(
            team_config_dir(tmp.path())
                .join("replenishment_context.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("review_policy.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("escalation_policy.md")
                .exists()
        );
        // kanban-md creates board/ directory; fallback creates kanban.md
        let config = team_config_dir(tmp.path());
        assert!(config.join("board").is_dir() || config.join("kanban.md").exists());
    }

    #[test]
    fn init_team_refuses_if_exists() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "simple", None, None, false).unwrap();
        let result = init_team(tmp.path(), "simple", None, None, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    const COMMENTED: &str = "\
# top comment
name: old # keep
roles:
  # the human
  - name: human
    role_type: user
  # lead
  - name: architect   # inline
    role_type: architect
    agent: claude # backend
    model: sonnet
    effort: high
    instances: 1
    instance_overrides:
      architect-1:
        model: opus
        effort: low
  - name: engineer
    role_type: engineer
    agent: claude
    model: 'haiku'
    instances: 2
";

    #[test]
    fn backend_override_preserves_comments_and_untouched_lines() {
        let (out, _) = override_template_backend_with_notes(COMMENTED, "codex").unwrap();
        let before: Vec<&str> = COMMENTED.lines().collect();
        let after: Vec<&str> = out.lines().collect();
        // Every comment survives.
        for line in &before {
            if line.trim_start().starts_with('#') {
                assert!(after.contains(line), "lost comment line {line:?}");
            }
        }
        assert!(out.contains("agent: codex # backend"), "{out}");
        assert!(out.contains("name: old # keep"));
        // Only agent/model lines differ: effort pins stay (codex supports effort).
        for line in &before {
            if !after.contains(line) {
                let t = line.trim_start();
                assert!(
                    t.starts_with("agent:") || t.starts_with("model:"),
                    "unexpected change to {line:?}"
                );
            }
        }
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let roles = parsed["roles"].as_sequence().unwrap();
        assert!(roles[0].get("agent").is_none(), "user role untouched");
        assert_eq!(roles[1]["agent"].as_str(), Some("codex"));
        assert_eq!(roles[1]["effort"].as_str(), Some("high"));
        assert!(roles[1].get("model").is_none());
        assert_eq!(
            roles[1]["instance_overrides"]["architect-1"]["effort"].as_str(),
            Some("low")
        );
        assert!(
            roles[1]["instance_overrides"]["architect-1"]
                .get("model")
                .is_none()
        );
    }

    #[test]
    fn backend_override_reports_dropped_pins_and_keeps_valid_ones() {
        let (out, notes) = override_template_backend_with_notes(COMMENTED, "kiro").unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        // kiro has no effort control, and claude models do not apply.
        assert!(parsed["roles"][1].get("effort").is_none());
        assert!(parsed["roles"][1].get("model").is_none());
        let joined = notes.join("\n");
        assert!(joined.contains("dropped effort 'high'"), "{joined}");
        assert!(joined.contains("dropped model 'sonnet'"), "{joined}");
        assert!(joined.contains("role 'engineer'"), "{joined}");

        // Switching to another Claude spelling is not a backend change.
        let (same, notes) = override_template_backend_with_notes(COMMENTED, "claude-code").unwrap();
        assert!(notes.is_empty(), "{notes:?}");
        assert!(same.contains("model: sonnet") && same.contains("effort: high"));

        // A non-Claude model pin survives a switch to codex; a Claude one does not.
        let yaml =
            "roles:\n  - name: e\n    role_type: engineer\n    agent: kiro\n    model: gpt-5\n";
        let (out, _) = override_template_backend_with_notes(yaml, "codex").unwrap();
        assert!(out.contains("model: gpt-5"), "{out}");
        let (out, _) = override_template_backend_with_notes(&out, "claude").unwrap();
        assert!(!out.contains("model:"), "{out}");
    }

    #[test]
    fn init_agent_and_name_only_edit_expected_lines() {
        let tmp = tempfile::tempdir().unwrap();
        init_team_with_overrides(
            tmp.path(),
            "simple",
            Some("a: b # c"),
            Some("codex"),
            false,
            None,
        )
        .unwrap();
        let out = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        let template = include_str!("templates/team_simple.yaml");
        let after: Vec<&str> = out.lines().collect();
        let mut changed = 0;
        for line in template.lines() {
            if !after.contains(&line) {
                let t = line.trim_start();
                assert!(
                    t.starts_with("name: my-project")
                        || t.starts_with("agent:")
                        || t.starts_with("model:"),
                    "unexpected change to {line:?}"
                );
                changed += 1;
            }
        }
        assert!(changed > 0);
        assert_eq!(
            serde_yaml::from_str::<serde_yaml::Value>(&out).unwrap()["name"].as_str(),
            Some("a: b # c")
        );
        // Effort pins from the template survive the switch.
        assert!(out.contains("effort: medium"));
    }

    #[test]
    fn set_template_name_quotes_special_names() {
        let template = "name: old\nroles: []\n";
        for name in [
            "a: b", "x # y", "-lead", "[x]", "{x}", "plain", "'q'", "123", "true",
        ] {
            let out = set_template_name(template, name).unwrap();
            let parsed: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
            assert_eq!(parsed["name"].as_str(), Some(name), "name {name:?}");
            assert!(parsed["roles"].is_sequence());
        }
    }

    #[test]
    fn set_template_name_rejects_empty_and_control_chars() {
        assert!(set_template_name("name: old\n", "  ").is_err());
        assert!(set_template_name("name: old\n", "a\nb: c").is_err());
    }

    #[test]
    fn init_team_with_special_project_name_loads() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "solo", Some("my: proj # x"), None, false).unwrap();
        let yaml =
            std::fs::read_to_string(team_config_dir(tmp.path()).join(TEAM_CONFIG_FILE)).unwrap();
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed["name"].as_str(), Some("my: proj # x"));
    }

    #[test]
    fn init_from_template_copies_files() {
        let _env_lock = env_lock();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.path());

        let template_dir = home.path().join(".batty").join("templates").join("custom");
        std::fs::create_dir_all(template_dir.join("board")).unwrap();
        std::fs::write(template_dir.join("team.yaml"), "name: custom\nroles: []\n").unwrap();
        std::fs::write(template_dir.join("architect.md"), "# Architect\n").unwrap();
        std::fs::write(template_dir.join("board").join("task.md"), "task\n").unwrap();

        let created = init_from_template(project.path(), "custom").unwrap();

        assert!(!created.is_empty());
        assert_eq!(
            std::fs::read_to_string(team_config_path(project.path())).unwrap(),
            "name: custom\nroles: []\n"
        );
        assert!(
            team_config_dir(project.path())
                .join("architect.md")
                .exists()
        );
        assert!(
            team_config_dir(project.path())
                .join("board")
                .join("task.md")
                .exists()
        );
    }

    #[test]
    fn init_from_template_restores_project_root_assets() {
        let _env_lock = env_lock();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.path());

        let template_dir = home
            .path()
            .join(".batty")
            .join("templates")
            .join("cleanroom");
        let export_root = template_dir.join(TEMPLATE_PROJECT_ROOT_DIR);
        std::fs::create_dir_all(export_root.join("analysis")).unwrap();
        std::fs::create_dir_all(export_root.join("implementation")).unwrap();
        std::fs::create_dir_all(export_root.join("planning")).unwrap();
        std::fs::write(template_dir.join("team.yaml"), "name: custom\nroles: []\n").unwrap();
        std::fs::write(template_dir.join("batty_decompiler.md"), "# Decompiler\n").unwrap();
        std::fs::write(export_root.join("PARITY.md"), "# Parity\n").unwrap();
        std::fs::write(export_root.join("SPEC.md"), "# Spec\n").unwrap();
        std::fs::write(
            export_root.join("planning").join("cleanroom-process.md"),
            "# Process\n",
        )
        .unwrap();

        let created = init_from_template(project.path(), "cleanroom").unwrap();

        assert!(!created.is_empty());
        assert_eq!(
            std::fs::read_to_string(team_config_path(project.path())).unwrap(),
            "name: custom\nroles: []\n"
        );
        assert!(
            team_config_dir(project.path())
                .join("batty_decompiler.md")
                .exists()
        );
        assert!(project.path().join("analysis").is_dir());
        assert!(project.path().join("implementation").is_dir());
        assert_eq!(
            std::fs::read_to_string(project.path().join("PARITY.md")).unwrap(),
            "# Parity\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("SPEC.md")).unwrap(),
            "# Spec\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("planning").join("cleanroom-process.md"))
                .unwrap(),
            "# Process\n"
        );
    }

    #[test]
    fn init_from_template_missing_template_errors_with_available_list() {
        let _env_lock = env_lock();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.path());

        let templates_root = home.path().join(".batty").join("templates");
        std::fs::create_dir_all(templates_root.join("alpha")).unwrap();
        std::fs::create_dir_all(templates_root.join("beta")).unwrap();

        let error = init_from_template(project.path(), "missing").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("template 'missing' not found"));
        assert!(message.contains("alpha"));
        assert!(message.contains("beta"));
    }

    #[test]
    fn init_from_template_errors_when_templates_dir_is_missing() {
        let _env_lock = env_lock();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = EnvVarGuard::set("HOME", home.path());

        let error = init_from_template(project.path(), "missing").unwrap_err();
        assert!(error.to_string().contains("no templates directory found"));
    }

    #[test]
    fn init_team_large_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "large", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("instances: 3") || content.contains("instances: 5"));
    }

    #[test]
    fn init_team_solo_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "solo", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("role_type: engineer"));
        assert!(!content.contains("role_type: manager"));
    }

    #[test]
    fn init_team_pair_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "pair", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("role_type: architect"));
        assert!(content.contains("role_type: engineer"));
        assert!(!content.contains("role_type: manager"));
    }

    #[test]
    fn init_team_squad_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "squad", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("instances: 5"));
        assert!(content.contains("layout:"));
    }

    #[test]
    fn init_team_research_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "research", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("principal"));
        assert!(content.contains("sub-lead"));
        assert!(content.contains("researcher"));
        // Research-specific .md files installed
        assert!(
            team_config_dir(tmp.path())
                .join("research_lead.md")
                .exists()
        );
        assert!(team_config_dir(tmp.path()).join("sub_lead.md").exists());
        assert!(team_config_dir(tmp.path()).join("researcher.md").exists());
        // Generic files NOT installed
        assert!(!team_config_dir(tmp.path()).join("architect.md").exists());
    }

    #[test]
    fn init_team_software_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "software", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("tech-lead"));
        assert!(content.contains("backend-mgr"));
        assert!(content.contains("frontend-mgr"));
        assert!(content.contains("developer"));
        // Software-specific .md files installed
        assert!(team_config_dir(tmp.path()).join("tech_lead.md").exists());
        assert!(team_config_dir(tmp.path()).join("eng_manager.md").exists());
        assert!(team_config_dir(tmp.path()).join("developer.md").exists());
    }

    #[test]
    fn init_team_batty_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "batty", None, None, false).unwrap();
        assert!(!created.is_empty());
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("batty-dev"));
        assert!(content.contains("role_type: architect"));
        assert!(content.contains("role_type: manager"));
        assert!(content.contains("instances: 4"));
        assert!(content.contains("batty_architect.md"));
        // Batty-specific .md files installed
        assert!(
            team_config_dir(tmp.path())
                .join("batty_architect.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("batty_manager.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("batty_engineer.md")
                .exists()
        );
        assert!(
            team_config_dir(tmp.path())
                .join("review_policy.md")
                .exists()
        );
        let manager_prompt =
            std::fs::read_to_string(team_config_dir(tmp.path()).join("batty_manager.md")).unwrap();
        let engineer_prompt =
            std::fs::read_to_string(team_config_dir(tmp.path()).join("batty_engineer.md")).unwrap();
        assert!(manager_prompt.contains("## Anti-Narration Rules"));
        assert!(manager_prompt.contains("Run the control-plane commands directly."));
        assert!(engineer_prompt.contains("## Anti-Narration Rules"));
        assert!(engineer_prompt.contains("Execute commands directly."));
    }

    #[test]
    fn init_team_cleanroom_template() {
        let tmp = tempfile::tempdir().unwrap();
        let created = init_team(tmp.path(), "cleanroom", None, None, false).unwrap();
        assert!(!created.is_empty());

        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(content.contains("decompiler"));
        assert!(content.contains("spec-writer"));
        assert!(content.contains("test-writer"));
        assert!(content.contains("implementer"));
        assert!(content.contains("batty_decompiler.md"));

        let config_dir = team_config_dir(tmp.path());
        assert!(config_dir.join("batty_decompiler.md").exists());
        assert!(config_dir.join("batty_spec_writer.md").exists());
        assert!(config_dir.join("batty_test_writer.md").exists());
        assert!(config_dir.join("batty_implementer.md").exists());
        assert!(tmp.path().join("analysis").is_dir());
        assert!(tmp.path().join("analysis").join("README.md").exists());
        assert!(tmp.path().join("implementation").is_dir());
        assert!(tmp.path().join("PARITY.md").exists());
        assert!(tmp.path().join("SPEC.md").exists());
        assert!(
            tmp.path()
                .join("planning")
                .join("cleanroom-process.md")
                .exists()
        );
    }

    #[test]
    fn init_team_cleanroom_template_scaffolds_parseable_parity_report() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "cleanroom", None, None, false).unwrap();

        let report = crate::team::parity::ParityReport::load(tmp.path()).unwrap();
        let summary = report.summary();

        assert_eq!(report.metadata.project, "clean-room-project");
        assert_eq!(report.metadata.source_platform, "zx-spectrum-z80");
        assert_eq!(summary.total_behaviors, 3);
        assert_eq!(summary.spec_complete, 0);
        assert_eq!(summary.verified_pass, 0);
    }

    #[test]
    fn init_with_agent_codex_sets_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let _created = init_team(tmp.path(), "simple", None, Some("codex"), false).unwrap();
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(
            content.contains("agent: codex"),
            "all agent fields should be codex"
        );
        assert!(
            !content.contains("agent: claude"),
            "no claude agents should remain"
        );
    }

    #[test]
    fn init_with_agent_kiro_sets_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let _created = init_team(tmp.path(), "pair", None, Some("kiro"), false).unwrap();
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(
            content.contains("agent: kiro"),
            "all agent fields should be kiro"
        );
        assert!(
            !content.contains("agent: claude"),
            "no claude agents should remain"
        );
        assert!(
            !content.contains("agent: codex"),
            "no codex agents should remain"
        );
    }

    #[test]
    fn init_default_agent_is_claude() {
        let tmp = tempfile::tempdir().unwrap();
        let _created = init_team(tmp.path(), "simple", None, None, false).unwrap();
        let content = std::fs::read_to_string(team_config_path(tmp.path())).unwrap();
        assert!(
            content.contains("agent: claude"),
            "default agent should be claude"
        );
    }

    #[test]
    fn export_template_creates_directory_and_copies_files() {
        let _env_lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", tmp.path());
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();
        std::fs::write(config_dir.join("architect.md"), "architect prompt\n").unwrap();

        let copied = export_template(&project_root, "demo-template").unwrap();
        let template_dir = templates_base_dir().unwrap().join("demo-template");

        assert_eq!(copied, 2);
        assert_eq!(
            std::fs::read_to_string(template_dir.join("team.yaml")).unwrap(),
            "name: demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(template_dir.join("architect.md")).unwrap(),
            "architect prompt\n"
        );
    }

    #[test]
    fn export_template_overwrites_existing() {
        let _env_lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", tmp.path());
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: first\n").unwrap();
        std::fs::write(config_dir.join("manager.md"), "v1\n").unwrap();

        export_template(&project_root, "demo-template").unwrap();

        std::fs::write(config_dir.join("team.yaml"), "name: second\n").unwrap();
        std::fs::write(config_dir.join("manager.md"), "v2\n").unwrap();

        let copied = export_template(&project_root, "demo-template").unwrap();
        let template_dir = templates_base_dir().unwrap().join("demo-template");

        assert_eq!(copied, 2);
        assert_eq!(
            std::fs::read_to_string(template_dir.join("team.yaml")).unwrap(),
            "name: second\n"
        );
        assert_eq!(
            std::fs::read_to_string(template_dir.join("manager.md")).unwrap(),
            "v2\n"
        );
    }

    #[test]
    fn export_template_missing_team_yaml_errors() {
        let _env_lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", tmp.path());
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(team_config_dir(&project_root)).unwrap();

        let error = export_template(&project_root, "demo-template").unwrap_err();

        assert!(error.to_string().contains("team config missing"));
    }

    #[test]
    fn export_template_includes_cleanroom_project_assets() {
        let _env_lock = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let _home = EnvVarGuard::set("HOME", tmp.path());
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(project_root.join("analysis")).unwrap();
        std::fs::create_dir_all(project_root.join("implementation")).unwrap();
        std::fs::create_dir_all(project_root.join("planning")).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();
        std::fs::write(config_dir.join("batty_decompiler.md"), "prompt\n").unwrap();
        std::fs::write(
            project_root.join("analysis").join("README.md"),
            "# Analysis\n",
        )
        .unwrap();
        std::fs::write(project_root.join("PARITY.md"), "# Parity\n").unwrap();
        std::fs::write(project_root.join("SPEC.md"), "# Spec\n").unwrap();
        std::fs::write(
            project_root.join("planning").join("cleanroom-process.md"),
            "# Process\n",
        )
        .unwrap();

        let copied = export_template(&project_root, "cleanroom-template").unwrap();
        let template_dir = templates_base_dir().unwrap().join("cleanroom-template");
        let export_root = template_dir.join(TEMPLATE_PROJECT_ROOT_DIR);

        assert_eq!(copied, 6);
        assert_eq!(
            std::fs::read_to_string(template_dir.join("team.yaml")).unwrap(),
            "name: demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(template_dir.join("batty_decompiler.md")).unwrap(),
            "prompt\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_root.join("analysis").join("README.md")).unwrap(),
            "# Analysis\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_root.join("PARITY.md")).unwrap(),
            "# Parity\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_root.join("SPEC.md")).unwrap(),
            "# Spec\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_root.join("planning").join("cleanroom-process.md"))
                .unwrap(),
            "# Process\n"
        );
    }

    #[test]
    fn init_team_cleanroom_template_includes_multi_backend_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "cleanroom", None, None, false).unwrap();

        let config_dir = team_config_dir(tmp.path());
        let decompiler = std::fs::read_to_string(config_dir.join("batty_decompiler.md")).unwrap();
        let spec_writer = std::fs::read_to_string(config_dir.join("batty_spec_writer.md")).unwrap();
        let process =
            std::fs::read_to_string(tmp.path().join("planning").join("cleanroom-process.md"))
                .unwrap();
        let analysis_readme =
            std::fs::read_to_string(tmp.path().join("analysis").join("README.md")).unwrap();

        assert!(decompiler.contains("SkoolKit"));
        assert!(decompiler.contains("Ghidra"));
        assert!(decompiler.contains("NES"));
        assert!(decompiler.contains("Game Boy"));
        assert!(decompiler.contains("DOS"));
        assert!(decompiler.contains("analysis/README.md"));
        assert!(spec_writer.contains("normalized analysis artifact"));
        assert!(process.contains("## Backend Selection"));
        assert!(analysis_readme.contains("## Normalized Analysis Artifact"));
    }

    #[test]
    fn export_run_copies_requested_run_state_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        let tasks_dir = config_dir.join("board").join("tasks");
        let retrospectives_dir = project_root.join(".batty").join("retrospectives");
        let worktree_dir = project_root
            .join(".batty")
            .join("worktrees")
            .join("eng-1-1")
            .join(".codex")
            .join("sessions");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        std::fs::create_dir_all(&retrospectives_dir).unwrap();
        std::fs::create_dir_all(&worktree_dir).unwrap();

        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();
        std::fs::write(tasks_dir.join("001-task.md"), "---\nid: 1\n---\n").unwrap();
        std::fs::write(
            team_events_path(&project_root),
            "{\"event\":\"daemon_started\"}\n",
        )
        .unwrap();
        std::fs::write(daemon_log_path(&project_root), "daemon-log\n").unwrap();
        std::fs::write(orchestrator_log_path(&project_root), "orchestrator-log\n").unwrap();
        std::fs::write(retrospectives_dir.join("retro.md"), "# Retro\n").unwrap();
        std::fs::write(
            project_root.join(".batty").join("test_timing.jsonl"),
            "{\"task_id\":1}\n",
        )
        .unwrap();
        std::fs::write(worktree_dir.join("session.jsonl"), "secret\n").unwrap();

        let export_dir = export_run(&project_root).unwrap();

        assert_eq!(
            std::fs::read_to_string(export_dir.join("team.yaml")).unwrap(),
            "name: demo\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("board").join("tasks").join("001-task.md"))
                .unwrap(),
            "---\nid: 1\n---\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("events.jsonl")).unwrap(),
            "{\"event\":\"daemon_started\"}\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("daemon.log")).unwrap(),
            "daemon-log\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("orchestrator.log")).unwrap(),
            "orchestrator-log\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("retrospectives").join("retro.md")).unwrap(),
            "# Retro\n"
        );
        assert_eq!(
            std::fs::read_to_string(export_dir.join("test_timing.jsonl")).unwrap(),
            "{\"task_id\":1}\n"
        );
        assert!(!export_dir.join("worktrees").exists());
        assert!(!export_dir.join(".codex").exists());
        assert!(!export_dir.join("sessions").exists());
    }

    #[test]
    fn export_run_skips_missing_optional_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        let config_dir = team_config_dir(&project_root);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("team.yaml"), "name: demo\n").unwrap();

        let export_dir = export_run(&project_root).unwrap();

        assert!(export_dir.join("team.yaml").is_file());
        assert!(!export_dir.join("board").exists());
        assert!(!export_dir.join("events.jsonl").exists());
        assert!(!export_dir.join("daemon.log").exists());
        assert!(!export_dir.join("orchestrator.log").exists());
        assert!(!export_dir.join("retrospectives").exists());
        assert!(!export_dir.join("test_timing.jsonl").exists());
    }

    #[test]
    fn export_run_missing_team_yaml_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let project_root = tmp.path().join("project");
        std::fs::create_dir_all(team_config_dir(&project_root)).unwrap();

        let error = export_run(&project_root).unwrap_err();

        assert!(error.to_string().contains("team config missing"));
    }

    #[test]
    fn apply_init_overrides_sets_fields() {
        let yaml = include_str!("templates/team_simple.yaml");
        let ov = InitOverrides {
            orchestrator_pane: Some(false),
            auto_dispatch: Some(true),
            use_worktrees: Some(false),
            timeout_nudges: Some(false),
            standups: Some(false),
            auto_merge_enabled: Some(true),
            standup_interval_secs: Some(999),
            stall_threshold_secs: Some(123),
            review_nudge_threshold_secs: Some(456),
            review_timeout_secs: Some(789),
            nudge_interval_secs: Some(555),
            ..Default::default()
        };
        let result = apply_init_overrides(yaml, &ov);
        let doc: serde_yaml::Value = serde_yaml::from_str(&result).unwrap();
        let map = doc.as_mapping().unwrap();

        assert_eq!(
            map.get(serde_yaml::Value::String("orchestrator_pane".into()))
                .and_then(|v| v.as_bool()),
            Some(false)
        );

        let board = map
            .get(serde_yaml::Value::String("board".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            board
                .get(serde_yaml::Value::String("auto_dispatch".into()))
                .and_then(|v| v.as_bool()),
            Some(true)
        );

        let automation = map
            .get(serde_yaml::Value::String("automation".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            automation
                .get(serde_yaml::Value::String("timeout_nudges".into()))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            automation
                .get(serde_yaml::Value::String("standups".into()))
                .and_then(|v| v.as_bool()),
            Some(false)
        );

        let standup = map
            .get(serde_yaml::Value::String("standup".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            standup
                .get(serde_yaml::Value::String("interval_secs".into()))
                .and_then(|v| v.as_u64()),
            Some(999)
        );

        let workflow_policy = map
            .get(serde_yaml::Value::String("workflow_policy".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            workflow_policy
                .get(serde_yaml::Value::String("stall_threshold_secs".into()))
                .and_then(|v| v.as_u64()),
            Some(123)
        );
        assert_eq!(
            workflow_policy
                .get(serde_yaml::Value::String(
                    "review_nudge_threshold_secs".into()
                ))
                .and_then(|v| v.as_u64()),
            Some(456)
        );
        assert_eq!(
            workflow_policy
                .get(serde_yaml::Value::String("review_timeout_secs".into()))
                .and_then(|v| v.as_u64()),
            Some(789)
        );

        let auto_merge = workflow_policy
            .get(serde_yaml::Value::String("auto_merge".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        assert_eq!(
            auto_merge
                .get(serde_yaml::Value::String("enabled".into()))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
    }
    #[test]
    fn mixed_template_uses_codex_for_bounded_execution_roles() {
        let tmp = tempfile::tempdir().unwrap();
        init_team(tmp.path(), "mixed", None, None, false).unwrap();
        let config = crate::team::config::TeamConfig::load(&team_config_path(tmp.path())).unwrap();
        let options = crate::team::agent_options::all(&config).unwrap();
        assert_eq!(
            options
                .iter()
                .filter(|member| member.backend.as_deref() == Some("codex"))
                .count(),
            3
        );
        for member in options
            .iter()
            .filter(|member| member.backend.as_deref() == Some("codex"))
        {
            assert_eq!(member.effort.as_deref(), Some("medium"));
            assert!(member.model.is_none());
        }
        assert!(
            team_config_dir(tmp.path())
                .join("quality_specialist.md")
                .is_file()
        );
        let converted =
            override_template_backend(include_str!("templates/team_simple.yaml"), "codex").unwrap();
        let converted: crate::team::config::TeamConfig = serde_yaml::from_str(&converted).unwrap();
        assert!(
            crate::team::agent_options::all(&converted)
                .unwrap()
                .iter()
                .all(|member| member.backend.as_deref() == Some("codex") && member.model.is_none())
        );
    }
}
