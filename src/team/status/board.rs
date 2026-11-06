use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::task;

use super::super::daemon_state_path;
use super::super::review::ReviewQueueState;
use super::super::standup::MemberState;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StatusTaskEntry {
    pub(crate) id: u32,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) priority: String,
    pub(crate) claimed_by: Option<String>,
    pub(crate) review_owner: Option<String>,
    pub(crate) blocked_on: Option<String>,
    pub(crate) branch: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch_mismatch: Option<String>,
    pub(crate) next_action: Option<String>,
    pub(crate) test_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) latest_artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed_test_state: Option<String>,
}

pub(crate) fn format_failed_test_task(task: &StatusTaskEntry) -> Option<String> {
    let summary = task.test_summary.as_deref()?;
    let owner = task.claimed_by.as_deref().unwrap_or("unclaimed");
    let artifact = task.latest_artifact.as_deref().unwrap_or("none");
    let state = task.failed_test_state.as_deref().unwrap_or("owner-locked");
    Some(format!(
        "#{} {} (owner: {}; artifact: {}; {}): {}",
        task.id, task.title, owner, artifact, state, summary
    ))
}

fn verification_retry_rework(metadata: &crate::team::board::WorkflowMetadata) -> bool {
    metadata.tests_passed == Some(false)
        && metadata.outcome.as_deref() == Some("verification_retry_required")
        && !metadata.artifacts.is_empty()
}

fn failed_test_summary(metadata: &crate::team::board::WorkflowMetadata) -> Option<String> {
    metadata
        .test_results
        .clone()
        .filter(|results| results.failed > 0)
        .map(|results| results.failure_summary())
        .or_else(|| {
            (metadata.tests_passed == Some(false)).then(|| {
                if metadata.outcome.as_deref() == Some("verification_retry_required") {
                    "verification retry required".to_string()
                } else {
                    "tests failed".to_string()
                }
            })
        })
}

fn current_owner_active_on_task(
    task: &task::Task,
    daemon_state: &PersistedDaemonHealthState,
) -> bool {
    let Some(owner) = task.claimed_by.as_deref() else {
        return false;
    };
    daemon_state.active_tasks.get(owner) == Some(&task.id)
        && daemon_state.states.get(owner) == Some(&MemberState::Working)
}

pub(super) fn failed_test_state(
    task: &task::Task,
    retry_rework: bool,
    daemon_state: &PersistedDaemonHealthState,
) -> String {
    if retry_rework && !current_owner_active_on_task(task, daemon_state) {
        "dispatchable rework".to_string()
    } else {
        "owner-locked".to_string()
    }
}

pub(crate) fn board_status_task_queues(
    project_root: &Path,
) -> Result<(Vec<StatusTaskEntry>, Vec<StatusTaskEntry>)> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut active_tasks = Vec::new();
    let mut review_queue = Vec::new();
    let tasks = load_board_tasks_for_status(
        tasks_dir.parent().unwrap_or(&tasks_dir),
        "board_status_task_queues",
    )?;
    let github_feedback =
        crate::team::github_feedback::summarize_github_feedback_for_tasks(project_root, &tasks)
            .unwrap_or_default();
    let daemon_state = load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .unwrap_or_default()
        .unwrap_or_default();
    for task in &tasks {
        let inferred = infer_runtime_task_metadata(project_root, task);
        let branch_mismatch = task_branch_mismatch(project_root, task, &inferred);
        let review_state = super::super::review::classify_review_task(project_root, task, &tasks);
        let workflow_metadata = crate::team::board::read_workflow_metadata(&task.source_path).ok();
        let latest_artifact = workflow_metadata
            .as_ref()
            .and_then(|metadata| metadata.artifacts.last().cloned());
        let retry_rework = workflow_metadata
            .as_ref()
            .is_some_and(verification_retry_rework);
        let mut test_summary = workflow_metadata.as_ref().and_then(failed_test_summary);
        let mut blocked_on = task.blocked_on.clone();
        let mut next_action = match review_state {
            ReviewQueueState::Current => task.next_action.clone(),
            ReviewQueueState::Stale(stale) => Some(stale.status_next_action()),
        };
        if let Some(feedback) = github_feedback.failed.get(&task.id) {
            blocked_on = Some(feedback.blocked_on_summary());
            next_action = Some(feedback.next_action_summary());
            test_summary = Some(feedback.status_summary());
        } else if test_summary.is_none()
            && let Some(feedback) = github_feedback.passed.get(&task.id)
        {
            test_summary = Some(feedback.status_summary());
        }
        let failed_test_state = test_summary
            .as_ref()
            .map(|_| failed_test_state(task, retry_rework, &daemon_state));
        let entry = StatusTaskEntry {
            id: task.id,
            title: task.title.clone(),
            status: task.status.clone(),
            priority: task.priority.clone(),
            claimed_by: task.claimed_by.clone(),
            review_owner: task.review_owner.clone(),
            blocked_on,
            branch: task.branch.clone().or_else(|| inferred.branch.clone()),
            worktree_path: task
                .worktree_path
                .clone()
                .or_else(|| inferred.worktree_path.clone()),
            commit: task.commit.clone().or_else(|| inferred.commit.clone()),
            branch_mismatch,
            next_action,
            test_summary,
            latest_artifact,
            failed_test_state,
        };

        match task.status.as_str() {
            "in-progress" | "in_progress" => active_tasks.push(entry),
            "review" => review_queue.push(entry),
            _ => {}
        }
    }

    Ok((active_tasks, review_queue))
}
