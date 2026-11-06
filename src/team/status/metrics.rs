use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde::Serialize;
use tracing::warn;

use crate::task;

use super::super::config::RoleType;
use super::super::events;
use super::super::hierarchy::MemberInstance;
use super::*;

#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct WorkflowMetrics {
    pub board_state: WorkflowBoardState,
    pub runnable_count: u32,
    pub implementation_runnable_count: u32,
    pub blocked_count: u32,
    pub in_review_count: u32,
    pub actionable_review_count: u32,
    pub in_progress_count: u32,
    pub stale_in_progress_count: u32,
    pub aged_todo_count: u32,
    pub stale_review_count: u32,
    pub idle_with_runnable: Vec<String>,
    pub top_runnable_tasks: Vec<String>,
    pub blocked_dispatch_reasons: Vec<String>,
    pub blocked_task_summaries: Vec<String>,
    pub oldest_review_age_secs: Option<u64>,
    pub oldest_assignment_age_secs: Option<u64>,
    // Review pipeline metrics (computed from event log)
    pub auto_merge_count: u32,
    pub manual_merge_count: u32,
    pub direct_root_merge_count: u32,
    pub isolated_integration_merge_count: u32,
    pub direct_root_failure_count: u32,
    pub isolated_integration_failure_count: u32,
    pub auto_merge_rate: Option<f64>,
    pub rework_count: u32,
    pub rework_rate: Option<f64>,
    pub review_nudge_count: u32,
    pub review_escalation_count: u32,
    pub avg_review_latency_secs: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkflowBoardState {
    #[default]
    EmptyBoard,
    BlockedOnlyBoard,
    ReviewBacklogGated,
    RunnableBoard,
    ActiveBoard,
}

pub(super) fn format_age_compact(secs: Option<f64>) -> String {
    let Some(secs) = secs else {
        return "-".to_string();
    };
    let secs = secs.round() as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3_600)
    }
}

pub(super) fn format_rate_1(value: Option<f64>) -> String {
    value
        .map(|value| format!("{value:.1}"))
        .unwrap_or_else(|| "-".to_string())
}

pub(super) fn format_pct_0(value: Option<f64>) -> String {
    value
        .map(|value| format!("{:.0}%", value * 100.0))
        .unwrap_or_else(|| "-".to_string())
}

pub fn compute_metrics(board_dir: &Path, members: &[MemberInstance]) -> Result<WorkflowMetrics> {
    compute_metrics_with_aging(
        board_dir,
        members,
        crate::team::board::AgingThresholds::default(),
    )
}

/// Compute workflow metrics, preferring SQLite telemetry DB over JSONL events.
///
/// If a `db` connection is provided, review pipeline metrics come from SQLite.
/// Otherwise falls back to `events_path` (JSONL), or returns zero review metrics.
pub fn compute_metrics_with_telemetry(
    board_dir: &Path,
    members: &[MemberInstance],
    db: Option<&rusqlite::Connection>,
    events_path: Option<&Path>,
) -> Result<WorkflowMetrics> {
    compute_metrics_with_telemetry_and_aging(
        board_dir,
        members,
        crate::team::board::AgingThresholds::default(),
        db,
        events_path,
    )
}

fn compute_metrics_with_aging(
    board_dir: &Path,
    members: &[MemberInstance],
    thresholds: crate::team::board::AgingThresholds,
) -> Result<WorkflowMetrics> {
    compute_metrics_with_telemetry_and_aging(board_dir, members, thresholds, None, None)
}

pub(super) fn compute_metrics_with_telemetry_and_aging(
    board_dir: &Path,
    members: &[MemberInstance],
    thresholds: crate::team::board::AgingThresholds,
    db: Option<&rusqlite::Connection>,
    events_path: Option<&Path>,
) -> Result<WorkflowMetrics> {
    let board_metrics = compute_board_metrics(board_dir, members, thresholds)?;

    let review = if let Some(conn) = db {
        compute_review_metrics_from_db(conn)
    } else {
        compute_review_metrics(events_path)
    };

    Ok(WorkflowMetrics {
        board_state: board_metrics.board_state,
        runnable_count: board_metrics.runnable_count,
        implementation_runnable_count: board_metrics.implementation_runnable_count,
        blocked_count: board_metrics.blocked_count,
        in_review_count: board_metrics.in_review_count,
        actionable_review_count: board_metrics.actionable_review_count,
        in_progress_count: board_metrics.in_progress_count,
        stale_in_progress_count: board_metrics.stale_in_progress_count,
        aged_todo_count: board_metrics.aged_todo_count,
        stale_review_count: board_metrics.stale_review_count,
        idle_with_runnable: board_metrics.idle_with_runnable,
        top_runnable_tasks: board_metrics.top_runnable_tasks,
        blocked_dispatch_reasons: board_metrics.blocked_dispatch_reasons,
        blocked_task_summaries: board_metrics.blocked_task_summaries,
        oldest_review_age_secs: board_metrics.oldest_review_age_secs,
        oldest_assignment_age_secs: board_metrics.oldest_assignment_age_secs,
        auto_merge_count: review.auto_merge_count,
        manual_merge_count: review.manual_merge_count,
        direct_root_merge_count: review.direct_root_merge_count,
        isolated_integration_merge_count: review.isolated_integration_merge_count,
        direct_root_failure_count: review.direct_root_failure_count,
        isolated_integration_failure_count: review.isolated_integration_failure_count,
        auto_merge_rate: review.auto_merge_rate,
        rework_count: review.rework_count,
        rework_rate: review.rework_rate,
        review_nudge_count: review.review_nudge_count,
        review_escalation_count: review.review_escalation_count,
        avg_review_latency_secs: review.avg_review_latency_secs,
    })
}

pub fn compute_metrics_with_events(
    board_dir: &Path,
    members: &[MemberInstance],
    events_path: Option<&Path>,
) -> Result<WorkflowMetrics> {
    compute_metrics_with_telemetry(board_dir, members, None, events_path)
}

/// Board-only metrics (no event/review data).
struct BoardMetrics {
    board_state: WorkflowBoardState,
    runnable_count: u32,
    implementation_runnable_count: u32,
    blocked_count: u32,
    in_review_count: u32,
    actionable_review_count: u32,
    in_progress_count: u32,
    stale_in_progress_count: u32,
    aged_todo_count: u32,
    stale_review_count: u32,
    idle_with_runnable: Vec<String>,
    top_runnable_tasks: Vec<String>,
    blocked_dispatch_reasons: Vec<String>,
    blocked_task_summaries: Vec<String>,
    oldest_review_age_secs: Option<u64>,
    oldest_assignment_age_secs: Option<u64>,
}

fn compute_board_metrics(
    board_dir: &Path,
    members: &[MemberInstance],
    thresholds: crate::team::board::AgingThresholds,
) -> Result<BoardMetrics> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(BoardMetrics {
            board_state: WorkflowBoardState::EmptyBoard,
            runnable_count: 0,
            implementation_runnable_count: 0,
            blocked_count: 0,
            in_review_count: 0,
            actionable_review_count: 0,
            in_progress_count: 0,
            stale_in_progress_count: 0,
            aged_todo_count: 0,
            stale_review_count: 0,
            idle_with_runnable: Vec::new(),
            top_runnable_tasks: Vec::new(),
            blocked_dispatch_reasons: Vec::new(),
            blocked_task_summaries: Vec::new(),
            oldest_review_age_secs: None,
            oldest_assignment_age_secs: None,
        });
    }

    let tasks = load_board_tasks_for_status(board_dir, "compute_board_metrics")?;
    if tasks.is_empty() {
        return Ok(BoardMetrics {
            board_state: WorkflowBoardState::EmptyBoard,
            runnable_count: 0,
            implementation_runnable_count: 0,
            blocked_count: 0,
            in_review_count: 0,
            actionable_review_count: 0,
            in_progress_count: 0,
            stale_in_progress_count: 0,
            aged_todo_count: 0,
            stale_review_count: 0,
            idle_with_runnable: Vec::new(),
            top_runnable_tasks: Vec::new(),
            blocked_dispatch_reasons: Vec::new(),
            blocked_task_summaries: Vec::new(),
            oldest_review_age_secs: None,
            oldest_assignment_age_secs: None,
        });
    }

    let dispatchable_tasks = crate::team::resolver::dispatchable_tasks(board_dir)?;
    let dispatchable_task_ids: HashSet<u32> =
        dispatchable_tasks.iter().map(|task| task.id).collect();

    let now = SystemTime::now();
    let runnable_count = tasks
        .iter()
        .filter(|task| dispatchable_task_ids.contains(&task.id))
        .count() as u32;

    let blocked_count = tasks
        .iter()
        .filter(|task| task.status == "blocked" || task.blocked.is_some())
        .count() as u32;
    let in_review_count = tasks.iter().filter(|task| task.status == "review").count() as u32;
    let actionable_review_count = tasks
        .iter()
        .filter(|task| task.status == "review")
        .filter(|task| !is_review_task_explicitly_blocked(task))
        .count() as u32;
    let in_progress_count = tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "in-progress" | "in_progress"))
        .count() as u32;

    let oldest_review_age_secs = tasks
        .iter()
        .filter(|task| task.status == "review")
        .filter_map(|task| file_age_secs(&task.source_path, now))
        .max();
    let oldest_assignment_age_secs = tasks
        .iter()
        .filter(|task| task.claimed_by.is_some())
        .filter(|task| !matches!(task.status.as_str(), "done" | "archived"))
        .filter_map(|task| file_age_secs(&task.source_path, now))
        .max();

    // #711: "idle with runnable" must only count tasks an engineer could
    // actually take. A task owned by the architect (body "Owner: maya-lead")
    // or manager (frontmatter assignee: jordan-pm) is runnable-in-general but
    // not engineer-runnable. Counting those caused the status display and
    // standup report to warn "idle while runnable work exists" to engineers
    // who had nothing to pick up, and surfaced the same false signal in
    // jordan-pm's standup workflow-signals. Same classification pattern as
    // #709 on the dispatch-gap / planning-cycle paths.
    let non_engineer_names: HashSet<String> = members
        .iter()
        .filter(|member| member.role_type != RoleType::Engineer)
        .map(|member| member.name.clone())
        .collect();
    let engineer_runnable_count = dispatchable_tasks
        .iter()
        .filter(|task| crate::team::resolver::is_engineer_dispatchable(task, &non_engineer_names))
        .count() as u32;
    let idle_with_runnable =
        compute_idle_with_runnable(board_dir, members, &tasks, engineer_runnable_count);
    let top_runnable_tasks = top_runnable_task_summaries(&dispatchable_tasks, 3);
    let blocked_dispatch_reasons =
        blocked_dispatch_reason_summaries(&tasks, &dispatchable_task_ids, 3);
    let blocked_task_summaries = blocked_task_recipe_summaries(&tasks, 5);
    let board_state = classify_workflow_board_state(
        &tasks,
        runnable_count,
        blocked_count,
        actionable_review_count,
    );
    let aging = project_root_from_board_dir(board_dir)
        .and_then(|project_root| {
            crate::team::board::compute_task_aging(board_dir, project_root, thresholds).ok()
        })
        .unwrap_or_default();

    Ok(BoardMetrics {
        board_state,
        runnable_count,
        implementation_runnable_count: engineer_runnable_count,
        blocked_count,
        in_review_count,
        actionable_review_count,
        in_progress_count,
        stale_in_progress_count: aging.stale_in_progress.len() as u32,
        aged_todo_count: aging.aged_todo.len() as u32,
        stale_review_count: aging.stale_review.len() as u32,
        idle_with_runnable,
        top_runnable_tasks,
        blocked_dispatch_reasons,
        blocked_task_summaries,
        oldest_review_age_secs,
        oldest_assignment_age_secs,
    })
}

fn classify_workflow_board_state(
    tasks: &[task::Task],
    runnable_count: u32,
    blocked_count: u32,
    actionable_review_count: u32,
) -> WorkflowBoardState {
    if tasks
        .iter()
        .all(|task| matches!(task.status.as_str(), "done" | "archived"))
    {
        return WorkflowBoardState::EmptyBoard;
    }

    if runnable_count > 0 {
        return WorkflowBoardState::RunnableBoard;
    }

    if actionable_review_count > 0 {
        return WorkflowBoardState::ReviewBacklogGated;
    }

    if blocked_count > 0 {
        return WorkflowBoardState::BlockedOnlyBoard;
    }

    WorkflowBoardState::ActiveBoard
}

#[derive(Default)]
struct ReviewMetrics {
    auto_merge_count: u32,
    manual_merge_count: u32,
    direct_root_merge_count: u32,
    isolated_integration_merge_count: u32,
    direct_root_failure_count: u32,
    isolated_integration_failure_count: u32,
    auto_merge_rate: Option<f64>,
    rework_count: u32,
    rework_rate: Option<f64>,
    review_nudge_count: u32,
    review_escalation_count: u32,
    avg_review_latency_secs: Option<f64>,
}

fn compute_review_metrics(events_path: Option<&Path>) -> ReviewMetrics {
    let events = events_path
        .and_then(|path| events::read_events(path).ok())
        .unwrap_or_default();

    let mut auto_merge_count: u32 = 0;
    let mut manual_merge_count: u32 = 0;
    let mut direct_root_merge_count: u32 = 0;
    let mut isolated_integration_merge_count: u32 = 0;
    let mut direct_root_failure_count: u32 = 0;
    let mut isolated_integration_failure_count: u32 = 0;
    let mut rework_count: u32 = 0;
    let mut review_nudge_count: u32 = 0;
    let mut review_escalation_count: u32 = 0;

    // Track review enter/exit times per task for latency computation.
    // "task_completed" with a task field entering review; merged events exiting.
    let mut review_enter_ts: HashMap<String, u64> = HashMap::new();
    let mut review_latencies: Vec<f64> = Vec::new();

    for event in &events {
        match event.event.as_str() {
            "task_auto_merged" => {
                auto_merge_count += 1;
                match event.merge_mode.as_deref() {
                    Some("direct_root") => direct_root_merge_count += 1,
                    Some("isolated_integration") => isolated_integration_merge_count += 1,
                    _ => {}
                }
                if let Some(task_id) = &event.task {
                    if let Some(enter_ts) = review_enter_ts.remove(task_id) {
                        review_latencies.push((event.ts - enter_ts) as f64);
                    }
                }
            }
            "task_manual_merged" => {
                manual_merge_count += 1;
                match event.merge_mode.as_deref() {
                    Some("direct_root") => direct_root_merge_count += 1,
                    Some("isolated_integration") => isolated_integration_merge_count += 1,
                    _ => {}
                }
                if let Some(task_id) = &event.task {
                    if let Some(enter_ts) = review_enter_ts.remove(task_id) {
                        review_latencies.push((event.ts - enter_ts) as f64);
                    }
                }
            }
            "task_reworked" => {
                rework_count += 1;
            }
            "task_merge_failed" => match event.merge_mode.as_deref() {
                Some("direct_root") => direct_root_failure_count += 1,
                Some("isolated_integration") => isolated_integration_failure_count += 1,
                _ => {}
            },
            "review_nudge_sent" => {
                review_nudge_count += 1;
            }
            "review_escalated" => {
                review_escalation_count += 1;
            }
            "task_completed" => {
                if let Some(task_id) = &event.task {
                    review_enter_ts.insert(task_id.clone(), event.ts);
                }
            }
            _ => {}
        }
    }

    let total_merges = auto_merge_count + manual_merge_count;
    let auto_merge_rate = if total_merges > 0 {
        Some(auto_merge_count as f64 / total_merges as f64)
    } else {
        None
    };
    let total_reviewed = total_merges + rework_count;
    let rework_rate = if total_reviewed > 0 {
        Some(rework_count as f64 / total_reviewed as f64)
    } else {
        None
    };
    let avg_review_latency_secs = if review_latencies.is_empty() {
        None
    } else {
        Some(review_latencies.iter().sum::<f64>() / review_latencies.len() as f64)
    };

    ReviewMetrics {
        auto_merge_count,
        manual_merge_count,
        direct_root_merge_count,
        isolated_integration_merge_count,
        direct_root_failure_count,
        isolated_integration_failure_count,
        auto_merge_rate,
        rework_count,
        rework_rate,
        review_nudge_count,
        review_escalation_count,
        avg_review_latency_secs,
    }
}

/// Compute review pipeline metrics from the SQLite telemetry database.
fn compute_review_metrics_from_db(conn: &rusqlite::Connection) -> ReviewMetrics {
    let row = match crate::team::telemetry_db::query_review_metrics(conn) {
        Ok(row) => row,
        Err(error) => {
            warn!(error = %error, "failed to query review metrics from telemetry DB; returning zeros");
            return ReviewMetrics::default();
        }
    };

    let auto_merge_count = row.auto_merge_count as u32;
    let manual_merge_count = row.manual_merge_count as u32;
    let direct_root_merge_count = row.direct_root_merge_count as u32;
    let isolated_integration_merge_count = row.isolated_integration_merge_count as u32;
    let direct_root_failure_count = row.direct_root_failure_count as u32;
    let isolated_integration_failure_count = row.isolated_integration_failure_count as u32;
    let rework_count = row.rework_count as u32;
    let total_merges = auto_merge_count + manual_merge_count;
    let auto_merge_rate = if total_merges > 0 {
        Some(auto_merge_count as f64 / total_merges as f64)
    } else {
        None
    };
    let total_reviewed = total_merges + rework_count;
    let rework_rate = if total_reviewed > 0 {
        Some(rework_count as f64 / total_reviewed as f64)
    } else {
        None
    };

    ReviewMetrics {
        auto_merge_count,
        manual_merge_count,
        direct_root_merge_count,
        isolated_integration_merge_count,
        direct_root_failure_count,
        isolated_integration_failure_count,
        auto_merge_rate,
        rework_count,
        rework_rate,
        review_nudge_count: row.review_nudge_count as u32,
        review_escalation_count: row.review_escalation_count as u32,
        avg_review_latency_secs: row.avg_review_latency_secs,
    }
}

impl WorkflowBoardState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyBoard => "empty-board",
            Self::BlockedOnlyBoard => "blocked-only-board",
            Self::ReviewBacklogGated => "review-backlog-gated",
            Self::RunnableBoard => "runnable-board",
            Self::ActiveBoard => "active-board",
        }
    }
}
