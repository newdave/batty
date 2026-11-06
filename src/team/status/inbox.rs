use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use tracing::warn;

use crate::task;

use super::super::config::RoleType;
use super::super::hierarchy::MemberInstance;
use super::super::inbox;
use super::super::review::ReviewQueueState;
use super::super::supervisory_notice::{
    SupervisoryPressure, classify_supervisory_pressure_normalized, normalized_body,
};
use super::super::{TRIAGE_RESULT_FRESHNESS_SECONDS, now_unix};
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TriageBacklogState {
    pub(crate) count: usize,
    pub(crate) newest_result_ts: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OwnedTaskBuckets {
    pub(crate) active: Vec<u32>,
    pub(crate) review: Vec<u32>,
    pub(crate) stale_review: Vec<u32>,
}

pub(crate) fn triage_backlog_counts(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, usize> {
    let root = inbox::inboxes_root(project_root);
    let direct_reports = direct_reports_by_member(members);
    direct_reports
        .into_iter()
        .filter_map(|(member_name, reports)| {
            match delivered_direct_report_triage_state(&root, &member_name, &reports) {
                Ok(state) => Some((member_name, state.count)),
                Err(error) => {
                    warn!(member = %member_name, error = %error, "failed to compute lead triage backlog");
                    None
                }
            }
        })
        .collect()
}

pub(crate) fn direct_reports_by_member(members: &[MemberInstance]) -> HashMap<String, Vec<String>> {
    let mut direct_reports: HashMap<String, Vec<String>> = HashMap::new();
    for member in members {
        if let Some(parent) = &member.reports_to {
            direct_reports
                .entry(parent.clone())
                .or_default()
                .push(member.name.clone());
        }
    }
    direct_reports
}

pub(crate) fn delivered_direct_report_triage_count(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
) -> Result<usize> {
    Ok(delivered_direct_report_triage_state(inbox_root, member_name, direct_reports)?.count)
}

pub(crate) fn delivered_direct_report_triage_state(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
) -> Result<TriageBacklogState> {
    delivered_direct_report_triage_state_at(inbox_root, member_name, direct_reports, now_unix())
}

pub(crate) fn delivered_direct_report_triage_state_at(
    inbox_root: &Path,
    member_name: &str,
    direct_reports: &[String],
    now_ts: u64,
) -> Result<TriageBacklogState> {
    if direct_reports.is_empty() {
        return Ok(TriageBacklogState {
            count: 0,
            newest_result_ts: 0,
        });
    }

    let mut latest_outbound_by_report = HashMap::new();
    for report in direct_reports {
        let report_messages = inbox::all_messages(inbox_root, report)?;
        let latest_outbound = report_messages
            .iter()
            .filter_map(|(msg, _)| (msg.from == member_name).then_some(msg.timestamp))
            .max()
            .unwrap_or(0);
        latest_outbound_by_report.insert(report.as_str(), latest_outbound);
    }

    let member_messages = inbox::all_messages(inbox_root, member_name)?;
    let mut latest_pending_by_report: HashMap<String, u64> = HashMap::new();
    for (msg, delivered) in &member_messages {
        let is_fresh = now_ts.saturating_sub(msg.timestamp) <= TRIAGE_RESULT_FRESHNESS_SECONDS;
        let needs_triage = *delivered
            && is_fresh
            && direct_reports.iter().any(|report| report == &msg.from)
            && msg.timestamp
                > *latest_outbound_by_report
                    .get(msg.from.as_str())
                    .unwrap_or(&0);
        if needs_triage {
            latest_pending_by_report
                .entry(msg.from.clone())
                .and_modify(|ts| *ts = (*ts).max(msg.timestamp))
                .or_insert(msg.timestamp);
        }
    }

    Ok(TriageBacklogState {
        count: latest_pending_by_report.len(),
        newest_result_ts: latest_pending_by_report
            .values()
            .copied()
            .max()
            .unwrap_or(0),
    })
}

pub(crate) fn pending_inbox_counts(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, usize> {
    let root = inbox::inboxes_root(project_root);
    members
        .iter()
        .filter_map(|member| {
            let count = if matches!(member.role_type, RoleType::Architect | RoleType::Manager) {
                // For supervisors, exclude low-priority status rollups from the inbox
                // count (they would otherwise inflate the number and mask real pending
                // work). Unclassified messages such as direct requests from humans are
                // always counted; only the specific low-signal categories are dropped.
                match inbox::pending_messages(&root, &member.name) {
                    Ok(messages) => messages
                        .iter()
                        .filter(|msg| {
                            !matches!(
                                classify_supervisory_pressure_normalized(&normalized_body(
                                    &msg.body
                                )),
                                Some(
                                    SupervisoryPressure::StatusUpdate
                                        | SupervisoryPressure::ResolvedUpdate
                                        | SupervisoryPressure::RecoveryUpdate
                                        | SupervisoryPressure::IdleNudge
                                        | SupervisoryPressure::ReviewNudge
                                )
                            )
                        })
                        .count(),
                    Err(error) => {
                        warn!(member = %member.name, error = %error, "failed to read pending inbox messages");
                        return None;
                    }
                }
            } else {
                match inbox::pending_message_count(&root, &member.name) {
                    Ok(count) => count,
                    Err(error) => {
                        warn!(member = %member.name, error = %error, "failed to count pending inbox messages");
                        return None;
                    }
                }
            };
            Some((member.name.clone(), count))
        })
        .collect()
}

pub(super) fn classify_owned_task_status(status: &str) -> Option<bool> {
    match status {
        "done" | "archived" => None,
        "review" => Some(false),
        _ => Some(true),
    }
}

pub(crate) fn owned_task_buckets(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, OwnedTaskBuckets> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return HashMap::new();
    }

    let tasks = match load_board_tasks_for_status(
        tasks_dir.parent().unwrap_or(&tasks_dir),
        "owned_task_buckets",
    ) {
        Ok(tasks) => tasks,
        Err(error) => {
            warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for status");
            return HashMap::new();
        }
    };
    owned_task_buckets_from_tasks(project_root, members, &tasks)
}

/// Bucket already-loaded board tasks by owner (see [`owned_task_buckets`]).
pub(crate) fn owned_task_buckets_from_tasks(
    project_root: &Path,
    members: &[MemberInstance],
    tasks: &[task::Task],
) -> HashMap<String, OwnedTaskBuckets> {
    let member_names: HashSet<&str> = members.iter().map(|member| member.name.as_str()).collect();
    let mut owned = HashMap::<String, OwnedTaskBuckets>::new();

    for task in tasks {
        let Some(claimed_by) = task.claimed_by.as_deref() else {
            continue;
        };
        if !member_names.contains(claimed_by) {
            continue;
        }
        let Some(is_active) = classify_owned_task_status(task.status.as_str()) else {
            continue;
        };
        let owner = if is_active {
            claimed_by.to_string()
        } else {
            members
                .iter()
                .find(|member| member.name == claimed_by)
                .and_then(|member| member.reports_to.as_deref())
                .unwrap_or(claimed_by)
                .to_string()
        };
        let entry = owned.entry(owner).or_default();
        if is_active {
            entry.active.push(task.id);
        } else {
            match super::super::review::classify_review_task(project_root, task, tasks) {
                ReviewQueueState::Current => entry.review.push(task.id),
                ReviewQueueState::Stale(_) => entry.stale_review.push(task.id),
            }
        }
    }

    for buckets in owned.values_mut() {
        buckets.active.sort_unstable();
        buckets.review.sort_unstable();
        buckets.stale_review.sort_unstable();
    }

    owned
}

pub(crate) fn format_owned_tasks_summary(task_ids: &[u32]) -> String {
    match task_ids {
        [] => "-".to_string(),
        [task_id] => format!("#{task_id}"),
        [first, second] => format!("#{first},#{second}"),
        [first, second, rest @ ..] => format!("#{first},#{second},+{}", rest.len()),
    }
}
