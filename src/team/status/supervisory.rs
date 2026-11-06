use std::collections::HashMap;
use std::path::Path;

use super::super::config::RoleType;
use super::super::hierarchy::MemberInstance;
use super::super::supervisory_notice::{
    SupervisoryMemberActivity, SupervisoryPressureSnapshot, supervisory_pressure_snapshots,
};
use super::*;

#[cfg(test)]
pub(crate) fn format_agent_health_summary(health: &AgentHealthSummary) -> String {
    format_agent_health_summary_for_role(health, None)
}

pub(crate) fn format_agent_health_summary_for_role(
    health: &AgentHealthSummary,
    role_type: Option<RoleType>,
) -> String {
    let mut parts = Vec::new();
    if !health.backend_health.is_healthy() {
        parts.push(format!("B:{}", health.backend_health.as_str()));
    }
    if health.restart_count > 0 {
        parts.push(format!("r{}", health.restart_count));
    }
    if health.context_exhaustion_count > 0 {
        parts.push(format!("c{}", health.context_exhaustion_count));
    }
    if health.proactive_handoff_count > 0 {
        parts.push(format!("ph{}", health.proactive_handoff_count));
    }
    if health.delivery_failure_count > 0 {
        parts.push(format!("d{}", health.delivery_failure_count));
    }
    if health.supervisory_digest_count > 0 {
        parts.push(format!("sd{}", health.supervisory_digest_count));
    }
    if health.dispatch_fallback_count > 0 {
        let token = match health.dispatch_fallback_reason.as_deref() {
            Some(reason) => format!("fd{}:{reason}", health.dispatch_fallback_count),
            None => format!("fd{}", health.dispatch_fallback_count),
        };
        parts.push(token);
    }
    if health.stale_active_cleared_count > 0 {
        parts.push(format!("sa{}", health.stale_active_cleared_count));
    }
    if let Some(supervisory_token) = health.supervisory_status_token_for_role(role_type) {
        parts.push(supervisory_token);
    }
    if let Some(task_elapsed_secs) = health.task_elapsed_secs {
        parts.push(format!("t{}", format_health_duration(task_elapsed_secs)));
    }

    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

pub(crate) fn format_health_duration(task_elapsed_secs: u64) -> String {
    if task_elapsed_secs < 60 {
        format!("{task_elapsed_secs}s")
    } else if task_elapsed_secs < 3_600 {
        format!("{}m", task_elapsed_secs / 60)
    } else if task_elapsed_secs < 86_400 {
        format!("{}h", task_elapsed_secs / 3_600)
    } else {
        format!("{}d", task_elapsed_secs / 86_400)
    }
}

pub(super) fn fallback_supervisory_stall_summary(
    reason: Option<&str>,
    stall_secs: Option<u64>,
    role_type: Option<RoleType>,
) -> Option<String> {
    let role_label = role_type
        .and_then(supervisory_role_label)
        .or_else(|| reason.and_then(supervisory_role_label_from_reason))
        .unwrap_or("supervisory");
    match (reason, stall_secs) {
        (Some(reason), Some(stall_secs)) => Some(format!(
            "{role_label} stall: {} after {}",
            supervisory_reason_label(reason),
            format_health_duration(stall_secs)
        )),
        (Some(reason), None) => Some(format!(
            "{role_label} stall: {}",
            supervisory_reason_label(reason)
        )),
        (None, Some(stall_secs)) => Some(format!(
            "{role_label} stall after {}",
            format_health_duration(stall_secs)
        )),
        (None, None) => None,
    }
}

pub(super) fn merge_status_signal(
    signal: Option<String>,
    branch_mismatch_signal: Option<String>,
    stale_active_signal: Option<String>,
    stall_signal: Option<String>,
    supervisory_pressure: &SupervisoryPressureSnapshot,
    stale_review_backlog: usize,
) -> Option<String> {
    let stale_review_signal =
        (stale_review_backlog > 0).then(|| format!("stale review ({stale_review_backlog})"));
    let actionable_backlog_present = supervisory_pressure.actionable_count() > 0;
    let mut signals = Vec::new();
    if let Some(existing) = signal {
        signals.push(existing);
    }
    if let Some(branch_mismatch) = branch_mismatch_signal {
        signals.push(branch_mismatch);
    }
    if let Some(stale_active) = stale_active_signal {
        signals.push(stale_active);
    }
    if let Some(stall) = stall_signal
        && (!actionable_backlog_present || is_actionable_control_plane_stall(&stall))
    {
        signals.push(stall);
    }
    if let Some(summary) = supervisory_pressure.status_summary() {
        signals.push(summary);
    }
    if let Some(stale_review) = stale_review_signal {
        signals.push(stale_review);
    }
    if signals.is_empty() {
        None
    } else {
        Some(signals.join(", "))
    }
}

fn is_actionable_control_plane_stall(stall_signal: &str) -> bool {
    stall_signal.contains("stale review backlog")
        || stall_signal.contains("stale direct-report packets")
        || stall_signal.contains("stale dispatch gap")
        || stall_signal.contains("stale planning inbox")
}

pub(super) fn supervisory_role_label(role_type: RoleType) -> Option<&'static str> {
    match role_type {
        RoleType::Architect => Some("architect"),
        RoleType::Manager => Some("manager"),
        _ => None,
    }
}

pub(super) fn supervisory_role_label_from_reason(reason: &str) -> Option<&'static str> {
    if reason.contains("architect") {
        Some("architect")
    } else if reason.contains("manager") {
        Some("manager")
    } else {
        None
    }
}

pub(super) fn is_supervisory_stall_reason(reason: &str) -> bool {
    reason.starts_with("supervisory_") || reason.contains("architect") || reason.contains("manager")
}

pub(super) fn supervisory_status_token(reason: &str, role_label: Option<&str>) -> String {
    let role_token = match role_label {
        Some("architect") => "stall:architect",
        Some("manager") => "stall:manager",
        _ => "stall",
    };
    let detail_token = supervisory_slo_status_token(reason);
    format!("{role_token}:{detail_token}")
}

fn supervisory_slo_status_token(reason: &str) -> &'static str {
    if reason.ends_with("review_waiting") || reason.ends_with("review_backlog") {
        "review-wait-timeout"
    } else if reason.ends_with("dispatch_gap") {
        "dispatch-gap-pressure"
    } else if reason.ends_with("direct_report_packets")
        || reason.ends_with("inbox_batching")
        || reason.ends_with("planning_inbox")
    {
        "inbox-backlog-pressure"
    } else {
        "working-timeout"
    }
}

fn supervisory_reason_label(reason: &str) -> &'static str {
    if reason.ends_with("inbox_batching") {
        "inbox batching"
    } else if reason.ends_with("review_waiting") {
        "review waiting"
    } else if reason.ends_with("review_backlog") {
        "stale review backlog"
    } else if reason.ends_with("direct_report_packets") {
        "stale direct-report packets"
    } else if reason.ends_with("dispatch_gap") {
        "stale dispatch gap"
    } else if reason.ends_with("planning_inbox") {
        "stale planning inbox"
    } else if reason.ends_with("shim_activity_only") {
        "shim activity only"
    } else if reason.ends_with("status_only_output") {
        "status-only output"
    } else if reason.ends_with("stalled") {
        "working timeout"
    } else {
        "no actionable progress"
    }
}

pub(crate) fn supervisory_status_pressure(
    project_root: &Path,
    members: &[MemberInstance],
    session_running: bool,
    runtime_statuses: &HashMap<String, RuntimeMemberStatus>,
) -> HashMap<String, SupervisoryPressureSnapshot> {
    let activity = members
        .iter()
        .map(|member| {
            let idle = session_running
                && runtime_statuses
                    .get(&member.name)
                    .is_some_and(|runtime| runtime.state == "idle");
            (member.name.clone(), SupervisoryMemberActivity { idle })
        })
        .collect::<HashMap<_, _>>();
    supervisory_pressure_snapshots(project_root, members, &activity)
}
