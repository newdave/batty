use std::path::Path;

use serde::Serialize;

use super::super::daemon::MainSmokeState;
use super::super::daemon_state_path;
use super::*;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct TeamStatusJsonReport {
    pub(crate) team: String,
    pub(crate) session: String,
    pub(crate) running: bool,
    pub(crate) paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) main_smoke: Option<MainSmokeState>,
    pub(crate) watchdog: WatchdogStatus,
    pub(crate) health: TeamStatusHealth,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) publish_handoff: Option<crate::release::ReleasePublishHandoff>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) optional_subsystems: Option<Vec<OptionalSubsystemStatus>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) engineer_profiles:
        Option<Vec<crate::team::telemetry_db::EngineerPerformanceProfileRow>>,
    pub(crate) members: Vec<TeamStatusRow>,
}

pub(crate) struct TeamStatusJsonReportInput {
    pub(crate) team: String,
    pub(crate) session: String,
    pub(crate) session_running: bool,
    pub(crate) paused: bool,
    pub(crate) main_smoke: Option<MainSmokeState>,
    pub(crate) watchdog: WatchdogStatus,
    pub(crate) workflow_metrics: Option<WorkflowMetrics>,
    pub(crate) publish_handoff: Option<crate::release::ReleasePublishHandoff>,
    pub(crate) active_tasks: Vec<StatusTaskEntry>,
    pub(crate) review_queue: Vec<StatusTaskEntry>,
    pub(crate) optional_subsystems: Option<Vec<OptionalSubsystemStatus>>,
    pub(crate) engineer_profiles:
        Option<Vec<crate::team::telemetry_db::EngineerPerformanceProfileRow>>,
    pub(crate) members: Vec<TeamStatusRow>,
}

pub(crate) fn load_main_smoke_state(project_root: &Path) -> Option<MainSmokeState> {
    load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .ok()
        .flatten()
        .and_then(|state| state.main_smoke_state)
}

/// One line per background test job the daemon is running, with elapsed time.
pub(crate) fn load_running_test_summaries(project_root: &Path) -> Vec<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .ok()
        .flatten()
        .map(|state| state.running_tests)
        .unwrap_or_default()
        .iter()
        .map(|job| job.summary(now))
        .collect()
}

pub(crate) fn format_main_smoke_summary(main_smoke: &MainSmokeState) -> String {
    if main_smoke.broken {
        let broken_commit = main_smoke.broken_commit.as_deref().unwrap_or("unknown");
        let suspects = if main_smoke.suspects.is_empty() {
            "none".to_string()
        } else {
            main_smoke.suspects.join(", ")
        };
        let summary = main_smoke.summary.as_deref().unwrap_or("main smoke failed");
        format!("BROKEN by {broken_commit}; suspects: [{suspects}]; {summary}")
    } else {
        let commit = main_smoke
            .last_success_commit
            .as_deref()
            .unwrap_or("unknown");
        let summary = main_smoke.summary.as_deref().unwrap_or("main smoke passed");
        format!("healthy at {commit}; {summary}")
    }
}

pub(crate) fn format_publish_handoff_summary(
    handoff: &crate::release::ReleasePublishHandoff,
) -> String {
    let mut summary = format!("{} ({})", handoff.path, handoff.status);
    if !handoff.blocked_reasons.is_empty() {
        summary.push_str(": ");
        summary.push_str(&handoff.blocked_reasons.join("; "));
    }
    summary
}

pub(crate) fn build_team_status_json_report(
    input: TeamStatusJsonReportInput,
) -> TeamStatusJsonReport {
    let TeamStatusJsonReportInput {
        team,
        session,
        session_running,
        paused,
        main_smoke,
        watchdog,
        workflow_metrics,
        publish_handoff,
        active_tasks,
        review_queue,
        optional_subsystems,
        engineer_profiles,
        members,
    } = input;
    let health = build_team_status_health(&members, session_running, paused);
    TeamStatusJsonReport {
        team,
        session,
        running: session_running,
        paused,
        main_smoke,
        watchdog,
        health,
        workflow_metrics,
        publish_handoff,
        active_tasks,
        review_queue,
        optional_subsystems,
        engineer_profiles,
        members,
    }
}

pub(crate) fn format_engineer_profiles(
    profiles: &[crate::team::telemetry_db::EngineerPerformanceProfileRow],
) -> String {
    let mut lines = vec![
        "Engineer Profiles".to_string(),
        format!(
            "{:<16} {:>5} {:>10} {:>10} {:>10} {:>10}",
            "ROLE", "TASKS", "AVG_TIME", "LOC/HR", "FIRST_PASS", "CTX_FREQ"
        ),
    ];

    for profile in profiles {
        lines.push(format!(
            "{:<16} {:>5} {:>10} {:>10} {:>10} {:>10}",
            profile.role,
            profile.completed_tasks,
            format_age_compact(profile.avg_task_completion_secs),
            format_rate_1(profile.lines_per_hour),
            format_pct_0(profile.first_pass_test_rate),
            format_pct_0(profile.context_exhaustion_frequency),
        ));
    }

    lines.join("\n")
}

pub(crate) fn format_benched_engineers(state: &crate::team::bench::BenchState) -> Option<String> {
    if state.benched.is_empty() {
        return None;
    }

    let mut lines = vec![
        "Benched Engineers".to_string(),
        format!("{:<20} {:<28} {}", "ENGINEER", "SINCE", "REASON"),
    ];
    for (engineer, entry) in &state.benched {
        lines.push(format!(
            "{:<20} {:<28} {}",
            engineer,
            entry.timestamp,
            entry.reason.as_deref().unwrap_or("-"),
        ));
    }
    Some(lines.join("\n"))
}
