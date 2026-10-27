//! Team lifecycle, configuration and health commands.

use crate::{
    agent,
    cli::{self, NudgeCommand},
    team,
};
use anyhow::{Result, bail};
use dialoguer::{Confirm, Input, Select};
use std::path::Path;
fn confirm(prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

fn input_u64(prompt: &str, default: u64) -> Result<u64> {
    let s: String = Input::new()
        .with_prompt(prompt)
        .default(default.to_string())
        .interact_text()?;
    Ok(s.parse::<u64>().unwrap_or(default))
}

/// Templates that assign a backend to each role. Init keeps those choices
/// unless `--agent` is passed explicitly.
fn template_has_per_role_backends(template: cli::InitTemplate) -> bool {
    matches!(template, cli::InitTemplate::Mixed)
}

fn collect_init_overrides() -> Result<team::InitOverrides> {
    let mut ov = team::InitOverrides::default();

    println!();
    println!("── Orchestrator ──────────────────────────────────────────");
    println!("The orchestrator is a dedicated tmux pane that runs automated");
    println!("triage, review routing, dispatch-gap recovery, and standups.");
    ov.orchestrator_pane = Some(confirm("Enable orchestrator pane?", false)?);

    println!();
    println!("── Board & Dispatch ──────────────────────────────────────");
    println!("Auto-dispatch automatically assigns todo tasks from the board");
    println!("to idle engineers without manual intervention.");
    ov.auto_dispatch = Some(confirm("Enable auto-dispatch of board tasks?", true)?);

    println!();
    println!("── Engineer Worktrees ────────────────────────────────────");
    println!("When enabled, each engineer gets an isolated git worktree.");
    println!("New task assignments create a fresh branch in that worktree,");
    println!("keeping engineers from stepping on each other's changes.");
    ov.use_worktrees = Some(confirm("Enable git worktrees for engineers?", true)?);

    println!();
    println!("── Automation: Nudges & Standups ─────────────────────────");
    println!("Timeout nudges ping agents that appear stuck or idle for too long.");
    ov.timeout_nudges = Some(confirm("Enable timeout nudges?", true)?);

    println!("Standups periodically ask agents to report their status.");
    ov.standups = Some(confirm("Enable periodic standups?", true)?);

    println!();
    println!("── Automation: Interventions ──────────────────────────────");
    println!("These are daemon-driven actions that keep the team moving.");
    println!();

    println!("Triage: auto-routes new tasks to the right manager/engineer.");
    ov.triage_interventions = Some(confirm("Enable triage interventions?", true)?);

    println!("Review: nudges reviewers and escalates stale reviews.");
    ov.review_interventions = Some(confirm("Enable review interventions?", true)?);

    println!("Owned-task recovery: re-dispatches tasks stuck on a dead agent.");
    ov.owned_task_interventions = Some(confirm("Enable owned-task recovery?", true)?);

    println!("Manager dispatch: nudges managers when todo tasks pile up.");
    ov.manager_dispatch_interventions =
        Some(confirm("Enable manager dispatch interventions?", true)?);

    println!("Architect utilization: nudges the architect when engineers are idle.");
    ov.architect_utilization_interventions = Some(confirm(
        "Enable architect utilization interventions?",
        false,
    )?);

    println!();
    println!("── Auto-Merge ───────────────────────────────────────────");
    println!("When enabled, completed engineer branches that pass tests and");
    println!("score above a confidence threshold are merged automatically.");
    ov.auto_merge_enabled = Some(confirm("Enable auto-merge?", true)?);

    println!();
    println!("── Timing (seconds) ─────────────────────────────────────");
    println!("These control how often the daemon checks on agents and reviews.");
    println!();

    ov.standup_interval_secs = Some(input_u64(
        "Standup interval (secs, how often agents report status)",
        1800,
    )?);

    ov.nudge_interval_secs = Some(input_u64(
        "Architect nudge interval (secs, idle ping for architect)",
        3600,
    )?);

    ov.stall_threshold_secs = Some(input_u64(
        "Stall threshold (secs, agent considered stuck after this)",
        900,
    )?);

    ov.review_nudge_threshold_secs = Some(input_u64(
        "Review nudge threshold (secs, reviewer gets a reminder)",
        1800,
    )?);

    ov.review_timeout_secs = Some(input_u64(
        "Review timeout (secs, stale review gets escalated)",
        7200,
    )?);

    println!();

    Ok(ov)
}

pub(super) fn run_init(
    root: &Path,
    template: Option<cli::InitTemplate>,
    from: Option<String>,
    force: bool,
    agent: Option<String>,
) -> Result<()> {
    // Validate agent name if provided
    if let Some(ref name) = agent {
        if agent::adapter_from_name(name).is_none() {
            bail!(
                "unknown agent backend '{name}'. Supported: {}",
                agent::KNOWN_AGENT_NAMES.join(", ")
            );
        }
    }
    let created = if let Some(template_name) = from.as_deref() {
        team::init_from_template(root, template_name)?
    } else {
        let template_choice = template.unwrap_or(cli::InitTemplate::Simple);
        let template_name = match template_choice {
            cli::InitTemplate::Solo => "solo",
            cli::InitTemplate::Pair => "pair",
            cli::InitTemplate::Simple => "simple",
            cli::InitTemplate::Mixed => "mixed",
            cli::InitTemplate::Squad => "squad",
            cli::InitTemplate::Large => "large",
            cli::InitTemplate::Research => "research",
            cli::InitTemplate::Software => "software",
            cli::InitTemplate::Cleanroom => "cleanroom",
            cli::InitTemplate::Batty => "batty",
            cli::InitTemplate::Python => "python",
        };
        // Interactive prompts for project name and agent
        let default_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "my-project".to_string());
        let project_name: String = Input::new()
            .with_prompt("Project name")
            .default(default_name)
            .interact_text()?;

        let selected_agent = if let Some(agent_name) = agent.as_deref() {
            Some(agent_name)
        } else if template_has_per_role_backends(template_choice) {
            println!(
                "Keeping the template's agent backend for each role (pass --agent to use one backend for every role)."
            );
            None
        } else {
            let agents = agent::KNOWN_AGENT_NAMES;
            let default_idx = agents.iter().position(|&a| a == "claude").unwrap_or(0);
            let agent_idx = Select::new()
                .with_prompt("Agent backend")
                .items(agents)
                .default(default_idx)
                .interact()?;
            Some(agents[agent_idx])
        };

        let overrides = collect_init_overrides()?;

        team::init_team_with_overrides(
            root,
            template_name,
            Some(&project_name),
            selected_agent,
            force,
            Some(&overrides),
        )?
    };
    println!("Initialized team config ({} files):", created.len());
    for path in &created {
        println!("  {}", path.display());
    }
    println!();
    println!("Edit .batty/team_config/team.yaml to configure your team.");
    println!("Then run: batty start");
    Ok(())
}

pub(super) fn run_start(root: &Path, attach: bool, quiet: bool) -> Result<()> {
    let session = team::start_team(root, attach)?;
    if !attach && !quiet {
        println!("Team session started: {session}");
        println!("Run `batty attach` to connect.");
    }
    Ok(())
}

pub(super) fn run_stop(root: &Path) -> Result<()> {
    team::stop_team(root)?;
    println!("Team session stopped.");
    Ok(())
}

pub(super) fn run_attach(root: &Path) -> Result<()> {
    team::attach_team(root)?;
    Ok(())
}

pub(super) fn run_status(root: &Path, json: bool, detail: bool, health: bool) -> Result<()> {
    team::team_status(root, json, detail, health)?;
    Ok(())
}

pub(super) fn run_bench(root: &Path, engineer: String, reason: Option<String>) -> Result<()> {
    let entry = team::bench::bench_engineer(root, &engineer, reason.as_deref())?;
    println!(
        "Benched {} at {}{}",
        engineer,
        entry.timestamp,
        entry
            .reason
            .as_deref()
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default()
    );
    Ok(())
}

pub(super) fn run_unbench(root: &Path, engineer: String) -> Result<()> {
    if team::bench::unbench_engineer(root, &engineer)? {
        println!("Unbenched {engineer}.");
    } else {
        println!("{engineer} was not benched.");
    }
    Ok(())
}

pub(super) fn run_validate(root: &Path, show_checks: bool) -> Result<()> {
    team::validate_team(root, show_checks)?;
    Ok(())
}

pub(super) fn run_config(root: &Path, json: bool) -> Result<()> {
    let config_path = team::team_config_path(root);
    let team_config = team::config::TeamConfig::load_existing(&config_path)?;
    if json {
        let members = team::hierarchy::resolve_hierarchy(&team_config)?;
        println!(
            "{}",
            team_config.to_inspection_json(&config_path, members.len())?
        );
    } else {
        println!("Config: {}", config_path.display());
        println!("Team: {}", team_config.name);
        println!("Roles: {}", team_config.roles.len());
        let members = team::hierarchy::resolve_hierarchy(&team_config)?;
        println!("Total members: {}", members.len());
        println!(
            "Board rotation threshold: {}",
            team_config.board.rotation_threshold
        );
        println!("Board auto-dispatch: {}", team_config.board.auto_dispatch);
        println!("Standup interval: {}s", team_config.standup.interval_secs);
        println!(
            "Automation: timeout_nudges={}, standups={}, failure_patterns={}, triage={}, review={}, owned_tasks={}, manager_dispatch={}, architect_utilization={}",
            team_config.automation.timeout_nudges,
            team_config.automation.standups,
            team_config.automation.failure_pattern_detection,
            team_config.automation.triage_interventions,
            team_config.automation.review_interventions,
            team_config.automation.owned_task_interventions,
            team_config.automation.manager_dispatch_interventions,
            team_config.automation.architect_utilization_interventions,
        );
        println!(
            "Workflow: mode={}, orchestrator_pane={}",
            team_config.workflow_mode.as_str(),
            team_config.orchestrator_pane
        );
    }
    Ok(())
}

pub(super) fn run_daemon_restart_if_stale(root: &Path, dry_run: bool) -> Result<()> {
    let report = team::restart_daemon_if_stale(root, dry_run)?;
    println!("{}", report.render());
    Ok(())
}

pub(super) fn run_nudge(root: &Path, command: NudgeCommand) -> Result<()> {
    match command {
        NudgeCommand::Disable { name } => {
            team::disable_nudge(root, name.marker_name())?;
            println!("Intervention '{}' disabled.", name.marker_name());
        }
        NudgeCommand::Enable { name } => {
            team::enable_nudge(root, name.marker_name())?;
            println!("Intervention '{}' re-enabled.", name.marker_name());
        }
        NudgeCommand::Status => {
            team::nudge_status(root)?;
        }
    }
    Ok(())
}

pub(super) fn run_pause(root: &Path) -> Result<()> {
    team::pause_team(root)?;
    println!("Nudges and standups paused. Run `batty resume` to resume.");
    Ok(())
}

pub(super) fn run_resume(root: &Path) -> Result<()> {
    team::resume_team(root)?;
    println!("Nudges and standups resumed.");
    Ok(())
}

pub(super) fn run_preflight(root: &Path, runtime: bool) -> Result<()> {
    let report = team::preflight::inspect(root, runtime)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if !report.diagnostics.is_empty() {
        bail!("preflight found configuration or runtime drift");
    }
    Ok(())
}

pub(super) fn run_backend_reset(root: &Path, member: String) -> Result<()> {
    team::preflight::reset_backend(root, &member)?;
    println!(
        "Backend configuration error cleared for {member}; a running daemon can recover this member."
    );
    Ok(())
}

pub(super) fn run_backend_probe(root: &Path, member: String, timeout_secs: u64) -> Result<()> {
    let result =
        team::backend_probe::run(root, &member, std::time::Duration::from_secs(timeout_secs))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

pub(super) fn run_scale(root: &Path, command: cli::ScaleCommand) -> Result<()> {
    team::scale::run(root, command)?;
    Ok(())
}

pub(super) fn run_reload(root: &Path) -> Result<()> {
    team::reload::request_topology_reload(root)?;
    println!("Topology reload requested. The daemon will pick it up on the next poll.");
    Ok(())
}

pub(super) fn run_doctor(root: &Path, fix: bool, yes: bool) -> Result<()> {
    print!("{}", team::doctor::run(root, fix, yes)?);
    Ok(())
}

pub(super) fn run_worktree(root: &Path, health: bool) -> Result<()> {
    if !health {
        bail!("worktree command requires --health");
    }
    print!("{}", team::worktree_health::run(root)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mixed_template_keeps_per_role_backends() {
        use cli::InitTemplate;
        assert!(template_has_per_role_backends(InitTemplate::Mixed));
        for template in [
            InitTemplate::Solo,
            InitTemplate::Pair,
            InitTemplate::Simple,
            InitTemplate::Squad,
            InitTemplate::Large,
            InitTemplate::Research,
            InitTemplate::Software,
            InitTemplate::Cleanroom,
            InitTemplate::Batty,
            InitTemplate::Python,
        ] {
            assert!(!template_has_per_role_backends(template), "{template:?}");
        }
    }
}
