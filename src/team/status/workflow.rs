use std::collections::HashSet;
use std::path::Path;
use std::time::SystemTime;

use tracing::warn;

use crate::task;

use super::super::config::{self, RoleType};
use super::super::hierarchy::MemberInstance;
use super::super::inbox;
use super::super::{team_config_dir, team_config_path, team_events_path};
use super::*;

pub fn format_metrics(metrics: &WorkflowMetrics) -> String {
    let idle = if metrics.idle_with_runnable.is_empty() {
        "-".to_string()
    } else {
        metrics.idle_with_runnable.join(", ")
    };
    let top_runnable = if metrics.top_runnable_tasks.is_empty() {
        "-".to_string()
    } else {
        metrics.top_runnable_tasks.join("; ")
    };
    let blocked_dispatch = if metrics.blocked_dispatch_reasons.is_empty() {
        "-".to_string()
    } else {
        metrics.blocked_dispatch_reasons.join("; ")
    };
    let blocked_tasks = if metrics.blocked_task_summaries.is_empty() {
        "-".to_string()
    } else {
        metrics.blocked_task_summaries.join("; ")
    };

    let auto_merge_rate_str = metrics
        .auto_merge_rate
        .map(|r| format!("{:.0}%", r * 100.0))
        .unwrap_or_else(|| "-".to_string());
    let rework_rate_str = metrics
        .rework_rate
        .map(|r| format!("{:.0}%", r * 100.0))
        .unwrap_or_else(|| "-".to_string());
    let avg_latency_str = metrics
        .avg_review_latency_secs
        .map(|secs| format_age(Some(secs as u64)))
        .unwrap_or_else(|| "-".to_string());
    let implementation_summary = crate::team::tact::parser::implementation_work_summary(
        metrics.implementation_runnable_count as usize,
        metrics.actionable_review_count as usize,
    );

    format!(
        "Workflow Metrics\n\
Board State: {}\n\
Runnable: {}\n\
Implementation Runnable: {}\n\
Blocked: {}\n\
In Review: {}\n\
Actionable Review: {}\n\
In Progress: {}\n\
Implementation Work: {}\n\
Aging Alerts: stale in-progress {} | aged todo {} | stale review {}\n\
Idle With Runnable: {}\n\
Top Runnable: {}\n\
Blocked Dispatch: {}\n\
Blocked Tasks: {}\n\
Oldest Review Age: {}\n\
Oldest Assignment Age: {}\n\n\
Review Pipeline\n\
Queue: {} | Avg Latency: {} | Auto-merge Rate: {} | Rework Rate: {}\n\
Auto: {} | Manual: {} | Rework: {} | Nudges: {} | Escalations: {}\n\
Merge Modes: direct ok {} / fail {} | isolated ok {} / fail {}",
        metrics.board_state.as_str(),
        metrics.runnable_count,
        metrics.implementation_runnable_count,
        metrics.blocked_count,
        metrics.in_review_count,
        metrics.actionable_review_count,
        metrics.in_progress_count,
        implementation_summary,
        metrics.stale_in_progress_count,
        metrics.aged_todo_count,
        metrics.stale_review_count,
        idle,
        top_runnable,
        blocked_dispatch,
        blocked_tasks,
        format_age(metrics.oldest_review_age_secs),
        format_age(metrics.oldest_assignment_age_secs),
        metrics.in_review_count,
        avg_latency_str,
        auto_merge_rate_str,
        rework_rate_str,
        metrics.auto_merge_count,
        metrics.manual_merge_count,
        metrics.rework_count,
        metrics.review_nudge_count,
        metrics.review_escalation_count,
        metrics.direct_root_merge_count,
        metrics.direct_root_failure_count,
        metrics.isolated_integration_merge_count,
        metrics.isolated_integration_failure_count,
    )
}

pub(crate) fn format_review_blocks(review_queue: &[StatusTaskEntry]) -> Option<String> {
    let blocked = review_queue
        .iter()
        .filter_map(|task| {
            task.blocked_on.as_ref().map(|blocked_on| {
                let next_action = task
                    .next_action
                    .as_deref()
                    .unwrap_or("resolve the blocker, then retry review");
                format!(
                    "#{} {}: {blocked_on} Next: {next_action}",
                    task.id, task.title
                )
            })
        })
        .collect::<Vec<_>>();
    if blocked.is_empty() {
        return None;
    }

    let mut lines = vec!["Review Blocks".to_string()];
    lines.extend(blocked.into_iter().map(|line| format!("- {line}")));
    Some(lines.join("\n"))
}

pub(super) fn compute_idle_with_runnable(
    board_dir: &Path,
    members: &[MemberInstance],
    tasks: &[task::Task],
    runnable_count: u32,
) -> Vec<String> {
    if runnable_count == 0 {
        return Vec::new();
    }

    let busy_engineers: HashSet<&str> = tasks
        .iter()
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
        .filter_map(|task| task.claimed_by.as_deref())
        .collect();

    let pending_root = project_root_from_board_dir(board_dir).map(inbox::inboxes_root);
    let mut idle = members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer)
        .filter(|member| !busy_engineers.contains(member.name.as_str()))
        .filter(|member| {
            pending_root
                .as_ref()
                .and_then(|root| inbox::pending_message_count(root, &member.name).ok())
                .unwrap_or(0)
                == 0
        })
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    idle.sort();
    idle
}

pub(crate) fn is_review_task_explicitly_blocked(task: &task::Task) -> bool {
    task.blocked
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || task
            .blocked_on
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}

fn task_priority_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

pub(super) fn top_runnable_task_summaries(tasks: &[task::Task], limit: usize) -> Vec<String> {
    let mut runnable = tasks.iter().collect::<Vec<_>>();
    runnable.sort_by_key(|task| (task_priority_rank(&task.priority), task.id));
    runnable
        .into_iter()
        .take(limit)
        .map(|task| format!("#{} ({}) {}", task.id, task.priority, task.title))
        .collect()
}

pub(super) fn blocked_dispatch_reason_summaries(
    tasks: &[task::Task],
    dispatchable_task_ids: &HashSet<u32>,
    limit: usize,
) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "todo" | "backlog" | "runnable"))
        .filter(|task| !dispatchable_task_ids.contains(&task.id))
        .filter_map(|task| {
            crate::team::resolver::dispatch_blocking_reason(task, tasks)
                .map(|reason| format!("#{} {}: {}", task.id, task.title, reason))
        })
        .take(limit)
        .collect()
}

pub(super) fn blocked_task_recipe_summaries(tasks: &[task::Task], limit: usize) -> Vec<String> {
    tasks
        .iter()
        .filter(|task| task.status == "blocked" || task.blocked.is_some())
        .take(limit)
        .map(|task| {
            let reason = task
                .blocked_on
                .as_deref()
                .or(task.blocked.as_deref())
                .unwrap_or("blocked without recorded reason");
            let next = blocked_task_next_action(task, tasks, reason);
            format!("#{} {}: {} Next: {}", task.id, task.title, reason, next)
        })
        .collect()
}

fn blocked_task_next_action(task: &task::Task, tasks: &[task::Task], reason: &str) -> String {
    if let Some(dep_id) = task.depends_on.first() {
        let dep_status = tasks
            .iter()
            .find(|candidate| candidate.id == *dep_id)
            .map(|candidate| candidate.status.as_str())
            .unwrap_or("missing");
        return match dep_status {
            "done" | "archived" => "clear stale dependency blocker and requeue".to_string(),
            "blocked" => format!("unblock dependency #{dep_id} first"),
            "missing" => format!("restore or remove missing dependency #{dep_id}"),
            other => format!("finish dependency #{dep_id} ({other}) first"),
        };
    }
    if reason.contains("preserve_failed_reset_skipped") {
        return "inspect the named worktree, preserve dirty work, then clear the stale preservation blocker".to_string();
    }
    if reason.contains("release") || task.title.to_ascii_lowercase().contains("release") {
        return "finish current fix queue and run `batty release --readiness` before tagging"
            .to_string();
    }
    "record an explicit owner/command, then move to todo when executable".to_string()
}

pub(super) fn project_root_from_board_dir(board_dir: &Path) -> Option<&Path> {
    board_dir.parent()?.parent()?.parent()
}

pub(super) fn file_age_secs(path: &Path, now: SystemTime) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(modified)
        .ok()
        .map(|duration| duration.as_secs())
}

fn format_age(age_secs: Option<u64>) -> String {
    age_secs
        .map(|secs| format!("{secs}s"))
        .unwrap_or_else(|| "n/a".to_string())
}

pub(crate) fn workflow_metrics_section(
    project_root: &Path,
    members: &[MemberInstance],
) -> Option<(String, WorkflowMetrics)> {
    let config_path = team_config_path(project_root);
    if !workflow_metrics_enabled(&config_path) {
        return None;
    }

    let board_dir = team_config_dir(project_root).join("board");
    let events_path = team_events_path(project_root);

    // Try SQLite telemetry DB first, fall back to JSONL events.
    // Use read-only opener to avoid blocking on the daemon's write lock (#676).
    let db = crate::team::telemetry_db::open_readonly(project_root)
        .ok()
        .flatten();
    let events_fallback = if db.is_none() && events_path.is_file() {
        Some(events_path.as_path())
    } else {
        None
    };

    let thresholds = config::TeamConfig::load(&config_path)
        .map(|config| crate::team::board::AgingThresholds {
            stale_in_progress_hours: config.workflow_policy.stale_in_progress_hours,
            aged_todo_hours: config.workflow_policy.aged_todo_hours,
            stale_review_hours: config.workflow_policy.stale_review_hours,
        })
        .unwrap_or_default();

    match compute_metrics_with_telemetry_and_aging(
        &board_dir,
        members,
        thresholds,
        db.as_ref(),
        events_fallback,
    ) {
        Ok(metrics) => {
            let mut formatted = format_metrics(&metrics);

            // Append binary freshness line (#675).
            let freshness_line = binary_freshness_status_line(project_root);
            formatted.push('\n');
            formatted.push_str(&freshness_line);

            Some((formatted, metrics))
        }
        Err(error) => {
            warn!(path = %board_dir.display(), error = %error, "failed to compute workflow metrics");
            None
        }
    }
}

/// Compute the binary freshness status line for `batty status` (#675).
fn binary_freshness_status_line(project_root: &Path) -> String {
    let binary_path = match std::env::current_exe() {
        Ok(path) => path,
        Err(_) => return "Daemon Binary: unknown (cannot resolve exe)".to_string(),
    };
    let result = crate::team::daemon::health::binary_freshness::evaluate_binary_freshness(
        &binary_path,
        project_root,
    );
    match result {
        Ok(Some(report)) => {
            if report.fresh {
                return report.status_line();
            }
            match crate::team::daemon::health::binary_freshness::load_binary_refresh_state(
                project_root,
            ) {
                Ok(Some(state)) if state.matches_report(&report) => state.status_line(),
                _ => report.status_line(),
            }
        }
        Ok(None) => "Daemon Binary: n/a".to_string(),
        Err(_) => "Daemon Binary: unknown (check failed)".to_string(),
    }
}

pub(crate) fn workflow_metrics_enabled(config_path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };

    content.lines().any(|line| {
        let line = line.trim();
        matches!(
            line,
            "workflow_mode: hybrid"
                | "workflow_mode: workflow_first"
                | "workflow_mode: board_first"
        )
    })
}
