//! `batty repair`: unbench engineers, clear backend errors, ask the daemon to
//! restart stuck members and clear a stale in-flight merge.
//!
//! Planning ([`build_plan`]) is pure; [`gather_input`] reads project state and
//! [`apply_action`] performs the side effects through the same functions the
//! single-purpose commands use.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::{Result, bail};

use super::bench::{self, BenchEntry, BenchSource};
use super::config::{RoleType, TeamConfig};
use super::hierarchy::resolve_hierarchy;
use super::legacy_command_queue::{self, QueuedCommand};
use super::{events, preflight, team_config_path, team_events_path};
use crate::tmux;

/// Window of the event log scanned for stuck members.
const EVENT_TAIL_BYTES: u64 = 4 * 1024 * 1024;
/// Label for actions that concern the whole team rather than one member.
const TEAM_SCOPE: &str = "team";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    Unbench {
        member: String,
        source: BenchSource,
        reason: Option<String>,
    },
    ResetBackend {
        member: String,
    },
    /// Daemon running: enqueue a restart request for it to execute.
    RequestRestart {
        member: String,
        reason: String,
    },
    /// No daemon: nothing to do, the member relaunches on `batty start`.
    RestartOnStart {
        member: String,
        reason: String,
    },
    ClearStaleMerge {
        task_id: u32,
    },
}

impl RepairAction {
    fn scope(&self) -> &str {
        match self {
            Self::Unbench { member, .. }
            | Self::ResetBackend { member }
            | Self::RequestRestart { member, .. }
            | Self::RestartOnStart { member, .. } => member,
            Self::ClearStaleMerge { .. } => TEAM_SCOPE,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Unbench { source, reason, .. } => {
                let kind = match source {
                    BenchSource::Manual => "manual",
                    BenchSource::AutoPark => "auto-park",
                };
                match reason {
                    Some(reason) => format!("unbench ({kind}: {reason})"),
                    None => format!("unbench ({kind})"),
                }
            }
            Self::ResetBackend { .. } => "clear backend configuration error".to_string(),
            Self::RequestRestart { reason, .. } => format!("request daemon restart ({reason})"),
            Self::RestartOnStart { reason, .. } => {
                format!("will restart on next `batty start` ({reason})")
            }
            Self::ClearStaleMerge { task_id } => {
                format!("clear stale in-flight merge of task #{task_id}")
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepairPlan {
    pub actions: Vec<RepairAction>,
}

/// Project state the plan is built from.
#[derive(Debug, Clone, Default)]
pub struct PlanInput {
    /// Members in scope, in team order.
    pub members: Vec<String>,
    pub benched: BTreeMap<String, BenchEntry>,
    /// Members with a backend configuration error marker.
    pub backend_markers: BTreeSet<String>,
    pub daemon_running: bool,
    /// Member to the reason its runtime is stuck.
    pub stuck: BTreeMap<String, String>,
    /// Task of a merge left in verification by a daemon that is gone.
    pub stale_merge: Option<u32>,
}

pub fn build_plan(input: &PlanInput) -> RepairPlan {
    let mut actions = Vec::new();
    for member in &input.members {
        if let Some(entry) = input.benched.get(member) {
            actions.push(RepairAction::Unbench {
                member: member.clone(),
                source: entry.source,
                reason: entry.reason.clone(),
            });
        }
        if input.backend_markers.contains(member) {
            actions.push(RepairAction::ResetBackend {
                member: member.clone(),
            });
        }
        if let Some(reason) = input.stuck.get(member) {
            let (member, reason) = (member.clone(), reason.clone());
            actions.push(if input.daemon_running {
                RepairAction::RequestRestart { member, reason }
            } else {
                RepairAction::RestartOnStart { member, reason }
            });
        }
    }
    if !input.daemon_running
        && let Some(task_id) = input.stale_merge
    {
        actions.push(RepairAction::ClearStaleMerge { task_id });
    }
    RepairPlan { actions }
}

/// Plan lines grouped by member; team-wide actions come last.
pub fn render_plan(plan: &RepairPlan) -> String {
    if plan.actions.is_empty() {
        return "Nothing to repair.\n".to_string();
    }
    let mut out = String::from("Repair plan:\n");
    let mut scopes: Vec<&str> = Vec::new();
    for action in &plan.actions {
        if !scopes.contains(&action.scope()) {
            scopes.push(action.scope());
        }
    }
    for scope in scopes {
        out.push_str(&format!("  {scope}\n"));
        for action in plan.actions.iter().filter(|a| a.scope() == scope) {
            out.push_str(&format!("    - {}\n", action.describe()));
        }
    }
    out
}

/// Members whose runtime is stuck, from the event log (context exhaustion or
/// a stall the daemon has not yet restarted) and live pane state (`crashed`).
fn stuck_members(
    project_root: &Path,
    crashed: impl IntoIterator<Item = String>,
) -> BTreeMap<String, String> {
    let mut stuck = BTreeMap::new();
    let events = events::read_events_tail(&team_events_path(project_root), EVENT_TAIL_BYTES)
        .unwrap_or_default();
    for event in events {
        if event.event == "daemon_started" {
            stuck.clear();
            continue;
        }
        let Some(role) = event.role else { continue };
        match event.event.as_str() {
            "context_exhausted" => {
                stuck.insert(role, "context exhausted".to_string());
            }
            "stall_detected" => {
                stuck.insert(role, "stalled".to_string());
            }
            "agent_restarted" | "pane_respawned" => {
                stuck.remove(&role);
            }
            _ => {}
        }
    }
    for member in crashed {
        stuck.insert(member, "pane dead".to_string());
    }
    stuck
}

/// Read project state for a plan, optionally limited to one member.
pub fn gather_input(project_root: &Path, only: Option<&str>) -> Result<PlanInput> {
    let config = TeamConfig::load_existing(&team_config_path(project_root))?;
    let all: Vec<String> = resolve_hierarchy(&config)?
        .into_iter()
        .filter(|m| m.role_type != RoleType::User)
        .map(|m| m.name)
        .collect();
    if let Some(only) = only
        && !all.iter().any(|name| name == only)
    {
        bail!("unknown member '{only}'");
    }
    let members: Vec<String> = all
        .into_iter()
        .filter(|name| only.is_none_or(|only| only == name))
        .collect();

    let backend_dir = project_root.join(".batty/backend-configuration");
    let backend_markers = members
        .iter()
        .filter(|m| backend_dir.join(format!("{m}.json")).exists())
        .cloned()
        .collect();
    let benched = bench::bench_entries(project_root)?
        .into_iter()
        .filter(|(name, _)| members.contains(name))
        .collect();

    let daemon_running = super::daemon::running_daemon_pid(project_root).is_some();
    let mut stuck = BTreeMap::new();
    if daemon_running {
        let session = format!("batty-{}", config.name);
        let crashed: Vec<String> = if tmux::session_exists(&session) {
            super::status::list_runtime_member_statuses(&session)
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, status)| status.state == "crashed")
                .map(|(name, _)| name)
                .collect()
        } else {
            Vec::new()
        };
        stuck = stuck_members(project_root, crashed);
        stuck.retain(|name, _| members.contains(name));
    }
    let stale_merge = if only.is_none() {
        super::daemon::stale_in_flight_merge(project_root)
    } else {
        None
    };

    Ok(PlanInput {
        members,
        benched,
        backend_markers,
        daemon_running,
        stuck,
        stale_merge,
    })
}

/// Perform one action; returns a short description of what happened.
pub fn apply_action(project_root: &Path, action: &RepairAction) -> Result<String> {
    match action {
        RepairAction::Unbench { member, .. } => {
            bench::unbench_engineer(project_root, member)?;
            Ok("unbenched".to_string())
        }
        RepairAction::ResetBackend { member } => {
            preflight::reset_backend(project_root, member)?;
            Ok("backend configuration error cleared".to_string())
        }
        RepairAction::RequestRestart { member, reason } => {
            legacy_command_queue::enqueue_command(
                &legacy_command_queue::command_queue_path(project_root),
                &QueuedCommand::restart(member, reason),
            )?;
            Ok("restart requested".to_string())
        }
        RepairAction::RestartOnStart { .. } => Ok("deferred to next start".to_string()),
        RepairAction::ClearStaleMerge { .. } => super::clear_stale_merge(project_root),
    }
}

/// Whether to apply the plan, ask first, or refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmation {
    Proceed,
    Prompt,
    Refuse,
}

pub fn confirmation(yes: bool, stdin_is_tty: bool) -> Confirmation {
    if yes {
        Confirmation::Proceed
    } else if stdin_is_tty {
        Confirmation::Prompt
    } else {
        Confirmation::Refuse
    }
}

fn summarize(plan: &RepairPlan, failed: &BTreeSet<usize>) -> String {
    let mut counts = [0usize; 5];
    for (index, action) in plan.actions.iter().enumerate() {
        if failed.contains(&index) {
            continue;
        }
        let slot = match action {
            RepairAction::Unbench { .. } => 0,
            RepairAction::ResetBackend { .. } => 1,
            RepairAction::RequestRestart { .. } => 2,
            RepairAction::RestartOnStart { .. } => 3,
            RepairAction::ClearStaleMerge { .. } => 4,
        };
        counts[slot] += 1;
    }
    let mut parts = Vec::new();
    for (label, count) in [
        ("unbenched", counts[0]),
        ("reset backend", counts[1]),
        ("restart requested", counts[2]),
        ("restart deferred to next start", counts[3]),
    ] {
        if count > 0 {
            parts.push(format!("{label} {count}"));
        }
    }
    if counts[4] > 0 {
        parts.push("cleared stale merge".to_string());
    }
    if !failed.is_empty() {
        parts.push(format!("failed {}", failed.len()));
    }
    if parts.is_empty() {
        "nothing applied".to_string()
    } else {
        parts.join(", ")
    }
}

/// Plan, print and apply. Returns the number of failed actions.
pub fn run_with(
    project_root: &Path,
    only: Option<&str>,
    dry_run: bool,
    yes: bool,
    stdin_is_tty: bool,
    out: &mut impl Write,
    prompt: impl FnOnce() -> Result<bool>,
) -> Result<usize> {
    let plan = build_plan(&gather_input(project_root, only)?);
    write!(out, "{}", render_plan(&plan))?;
    if plan.actions.is_empty() {
        return Ok(0);
    }
    if dry_run {
        writeln!(out, "Dry run: no changes made.")?;
        return Ok(0);
    }
    match confirmation(yes, stdin_is_tty) {
        Confirmation::Proceed => {}
        Confirmation::Refuse => {
            out.flush()?;
            bail!("refusing to repair without a terminal to confirm; pass --yes or --dry-run");
        }
        Confirmation::Prompt => {
            out.flush()?;
            if !prompt()? {
                writeln!(out, "Repair aborted.")?;
                return Ok(0);
            }
        }
    }

    let mut failed = BTreeSet::new();
    for (index, action) in plan.actions.iter().enumerate() {
        match apply_action(project_root, action) {
            Ok(message) => writeln!(out, "{}: {message}", action.scope())?,
            Err(error) => {
                failed.insert(index);
                writeln!(out, "{}: FAILED: {error:#}", action.scope())?;
            }
        }
    }
    writeln!(out, "Summary: {}", summarize(&plan, &failed))?;
    Ok(failed.len())
}

/// `batty repair` entry point using stdout/stdin.
pub fn run(project_root: &Path, only: Option<&str>, dry_run: bool, yes: bool) -> Result<()> {
    let failed = run_with(
        project_root,
        only,
        dry_run,
        yes,
        std::io::stdin().is_terminal(),
        &mut std::io::stdout(),
        || super::doctor::prompt_yes_no("Apply the repairs listed above? [y/N] ", false),
    )?;
    if failed > 0 {
        bail!("{failed} repair action(s) failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: BenchSource, reason: Option<&str>) -> BenchEntry {
        BenchEntry {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            reason: reason.map(str::to_string),
            source,
            task_id: None,
        }
    }

    fn write_project(root: &Path) -> Vec<String> {
        let dir = root.join(".batty").join("team_config");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("team.yaml"),
            "name: test\nagent: codex\nroles:\n  - name: architect\n    role_type: architect\n  - name: manager\n    role_type: manager\n  - name: engineer\n    role_type: engineer\n    instances: 2\n",
        )
        .unwrap();
        let config = TeamConfig::load(&dir.join("team.yaml")).unwrap();
        resolve_hierarchy(&config)
            .unwrap()
            .into_iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .map(|m| m.name)
            .collect()
    }

    fn marker(root: &Path, member: &str) -> std::path::PathBuf {
        let dir = root.join(".batty/backend-configuration");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{member}.json"));
        std::fs::write(&path, "{}").unwrap();
        path
    }

    fn run_capture(
        root: &Path,
        only: Option<&str>,
        dry_run: bool,
        yes: bool,
        tty: bool,
        answer: bool,
    ) -> (Result<usize>, String) {
        let mut out = Vec::new();
        let result = run_with(root, only, dry_run, yes, tty, &mut out, || Ok(answer));
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn plan_covers_every_step_in_member_order() {
        let input = PlanInput {
            members: vec!["eng-1".into(), "eng-2".into()],
            benched: BTreeMap::from([
                ("eng-1".into(), entry(BenchSource::Manual, Some("tired"))),
                ("eng-2".into(), entry(BenchSource::AutoPark, None)),
            ]),
            backend_markers: BTreeSet::from(["eng-2".to_string()]),
            daemon_running: true,
            stuck: BTreeMap::from([("eng-1".into(), "pane dead".into())]),
            stale_merge: Some(7),
        };
        let plan = build_plan(&input);
        assert_eq!(
            plan.actions,
            vec![
                RepairAction::Unbench {
                    member: "eng-1".into(),
                    source: BenchSource::Manual,
                    reason: Some("tired".into())
                },
                RepairAction::RequestRestart {
                    member: "eng-1".into(),
                    reason: "pane dead".into()
                },
                RepairAction::Unbench {
                    member: "eng-2".into(),
                    source: BenchSource::AutoPark,
                    reason: None
                },
                RepairAction::ResetBackend {
                    member: "eng-2".into()
                },
            ]
        );
    }

    #[test]
    fn stale_merge_and_restart_depend_on_daemon_state() {
        let mut input = PlanInput {
            members: vec!["eng-1".into()],
            stuck: BTreeMap::from([("eng-1".into(), "stalled".into())]),
            stale_merge: Some(3),
            ..PlanInput::default()
        };
        let plan = build_plan(&input);
        assert_eq!(
            plan.actions,
            vec![
                RepairAction::RestartOnStart {
                    member: "eng-1".into(),
                    reason: "stalled".into()
                },
                RepairAction::ClearStaleMerge { task_id: 3 },
            ]
        );
        input.daemon_running = true;
        let plan = build_plan(&input);
        assert!(
            !plan
                .actions
                .iter()
                .any(|a| matches!(a, RepairAction::ClearStaleMerge { .. }))
        );
        assert!(matches!(
            plan.actions[0],
            RepairAction::RequestRestart { .. }
        ));
    }

    #[test]
    fn empty_plan_renders_nothing_to_repair() {
        assert_eq!(
            render_plan(&build_plan(&PlanInput::default())),
            "Nothing to repair.\n"
        );
    }

    #[test]
    fn render_groups_actions_by_member() {
        let plan = RepairPlan {
            actions: vec![
                RepairAction::Unbench {
                    member: "eng-1".into(),
                    source: BenchSource::Manual,
                    reason: None,
                },
                RepairAction::ResetBackend {
                    member: "eng-1".into(),
                },
                RepairAction::ClearStaleMerge { task_id: 9 },
            ],
        };
        let text = render_plan(&plan);
        assert_eq!(
            text,
            "Repair plan:\n  eng-1\n    - unbench (manual)\n    - clear backend configuration error\n  team\n    - clear stale in-flight merge of task #9\n"
        );
    }

    #[test]
    fn confirmation_requires_yes_without_tty() {
        assert_eq!(confirmation(true, false), Confirmation::Proceed);
        assert_eq!(confirmation(true, true), Confirmation::Proceed);
        assert_eq!(confirmation(false, true), Confirmation::Prompt);
        assert_eq!(confirmation(false, false), Confirmation::Refuse);
    }

    #[test]
    fn stuck_members_tracks_events_until_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = team_events_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines = [
            r#"{"event":"context_exhausted","role":"eng-1","ts":1}"#,
            r#"{"event":"stall_detected","role":"eng-2","ts":2}"#,
            r#"{"event":"stall_detected","role":"eng-3","ts":3}"#,
            r#"{"event":"agent_restarted","role":"eng-3","ts":4}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();

        let stuck = stuck_members(tmp.path(), ["eng-4".to_string()]);
        assert_eq!(stuck["eng-1"], "context exhausted");
        assert_eq!(stuck["eng-2"], "stalled");
        assert_eq!(stuck["eng-4"], "pane dead");
        assert!(!stuck.contains_key("eng-3"));

        std::fs::write(
            &path,
            format!("{}\n{}", lines[0], r#"{"event":"daemon_started","ts":9}"#),
        )
        .unwrap();
        assert!(stuck_members(tmp.path(), []).is_empty());
    }

    #[test]
    fn gather_finds_benched_and_backend_marked_members() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        bench::bench_engineer(tmp.path(), &engineers[0], Some("manual")).unwrap();
        bench::bench_engineer_with(
            tmp.path(),
            &engineers[1],
            Some("parked"),
            BenchSource::AutoPark,
            Some(4),
        )
        .unwrap();
        marker(tmp.path(), &engineers[1]);

        let input = gather_input(tmp.path(), None).unwrap();
        assert_eq!(input.benched.len(), 2);
        assert_eq!(
            input.backend_markers,
            BTreeSet::from([engineers[1].clone()])
        );
        assert!(!input.daemon_running);
        assert!(input.stuck.is_empty());
    }

    #[test]
    fn member_filter_limits_scope_and_rejects_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        for e in &engineers {
            bench::bench_engineer(tmp.path(), e, None).unwrap();
        }
        let input = gather_input(tmp.path(), Some(&engineers[0])).unwrap();
        assert_eq!(input.members, vec![engineers[0].clone()]);
        assert_eq!(input.benched.len(), 1);
        assert!(input.benched.contains_key(&engineers[0]));
        assert!(gather_input(tmp.path(), Some("nobody")).is_err());
    }

    #[test]
    fn dry_run_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        bench::bench_engineer(tmp.path(), &engineers[0], None).unwrap();
        let path = marker(tmp.path(), &engineers[0]);

        let (result, out) = run_capture(tmp.path(), None, true, false, false, false);
        assert_eq!(result.unwrap(), 0);
        assert!(out.contains("Dry run"));
        assert!(out.contains("unbench (manual)"));
        assert_eq!(bench::bench_entries(tmp.path()).unwrap().len(), 1);
        assert!(path.exists());
    }

    #[test]
    fn yes_applies_unbench_and_backend_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        bench::bench_engineer(tmp.path(), &engineers[0], None).unwrap();
        bench::bench_engineer_with(tmp.path(), &engineers[1], None, BenchSource::AutoPark, None)
            .unwrap();
        let path = marker(tmp.path(), &engineers[0]);

        let (result, out) = run_capture(tmp.path(), None, false, true, false, false);
        assert_eq!(result.unwrap(), 0);
        assert!(bench::bench_entries(tmp.path()).unwrap().is_empty());
        assert!(!path.exists());
        assert!(out.contains("Summary: unbenched 2, reset backend 1"));
    }

    #[test]
    fn member_filter_leaves_other_engineers_benched() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        for e in &engineers {
            bench::bench_engineer(tmp.path(), e, None).unwrap();
        }
        let (result, _) = run_capture(tmp.path(), Some(&engineers[0]), false, true, false, false);
        assert_eq!(result.unwrap(), 0);
        let left = bench::bench_entries(tmp.path()).unwrap();
        assert_eq!(left.keys().collect::<Vec<_>>(), vec![&engineers[1]]);
    }

    #[test]
    fn no_tty_without_yes_refuses_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        bench::bench_engineer(tmp.path(), &engineers[0], None).unwrap();

        let (result, _) = run_capture(tmp.path(), None, false, false, false, true);
        assert!(result.unwrap_err().to_string().contains("--yes"));
        assert_eq!(bench::bench_entries(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn tty_prompt_decides() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        bench::bench_engineer(tmp.path(), &engineers[0], None).unwrap();

        let (result, out) = run_capture(tmp.path(), None, false, false, true, false);
        assert_eq!(result.unwrap(), 0);
        assert!(out.contains("Repair aborted"));
        assert_eq!(bench::bench_entries(tmp.path()).unwrap().len(), 1);

        let (result, _) = run_capture(tmp.path(), None, false, false, true, true);
        assert_eq!(result.unwrap(), 0);
        assert!(bench::bench_entries(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn nothing_to_repair_succeeds_without_confirmation() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let (result, out) = run_capture(tmp.path(), None, false, false, false, false);
        assert_eq!(result.unwrap(), 0);
        assert_eq!(out, "Nothing to repair.\n");
    }

    #[test]
    fn restart_action_enqueues_command_for_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        apply_action(
            tmp.path(),
            &RepairAction::RequestRestart {
                member: "eng-1".into(),
                reason: "pane dead".into(),
            },
        )
        .unwrap();
        let queued = legacy_command_queue::read_command_queue(
            &legacy_command_queue::command_queue_path(tmp.path()),
        )
        .unwrap();
        assert_eq!(queued.len(), 1);
        assert!(matches!(
            &queued[0],
            QueuedCommand::Restart { member, reason, .. }
                if member == "eng-1" && reason == "pane dead"
        ));
    }

    #[test]
    fn deferred_restart_touches_no_queue() {
        let tmp = tempfile::tempdir().unwrap();
        apply_action(
            tmp.path(),
            &RepairAction::RestartOnStart {
                member: "eng-1".into(),
                reason: "stalled".into(),
            },
        )
        .unwrap();
        assert!(!legacy_command_queue::command_queue_path(tmp.path()).exists());
    }

    #[test]
    fn clear_stale_merge_action_reports_nothing_in_flight() {
        let tmp = tempfile::tempdir().unwrap();
        write_project(tmp.path());
        let message =
            apply_action(tmp.path(), &RepairAction::ClearStaleMerge { task_id: 1 }).unwrap();
        assert!(message.contains("No in-flight merge"));
    }

    #[test]
    fn failed_action_is_reported_and_remaining_actions_run() {
        let tmp = tempfile::tempdir().unwrap();
        let engineers = write_project(tmp.path());
        bench::bench_engineer(tmp.path(), &engineers[1], None).unwrap();
        let plan = RepairPlan {
            actions: vec![
                RepairAction::ResetBackend {
                    member: "ghost".into(),
                },
                RepairAction::Unbench {
                    member: engineers[1].clone(),
                    source: BenchSource::Manual,
                    reason: None,
                },
            ],
        };
        assert!(apply_action(tmp.path(), &plan.actions[0]).is_err());
        assert!(apply_action(tmp.path(), &plan.actions[1]).is_ok());
        assert_eq!(
            summarize(&plan, &BTreeSet::from([0])),
            "unbenched 1, failed 1"
        );
    }
}
