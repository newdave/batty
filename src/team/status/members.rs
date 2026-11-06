use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Serialize;

use super::super::config::{self, RoleType};
use super::super::hierarchy::MemberInstance;
use super::super::supervisory_notice::SupervisoryPressureSnapshot;
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct RuntimeMemberStatus {
    pub(crate) state: String,
    pub(crate) signal: Option<String>,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamStatusRow {
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) role_type: String,
    pub(crate) agent: Option<String>,
    pub(crate) reports_to: Option<String>,
    pub(crate) state: String,
    pub(crate) pending_inbox: usize,
    pub(crate) triage_backlog: usize,
    pub(crate) active_owned_tasks: Vec<u32>,
    pub(crate) review_owned_tasks: Vec<u32>,
    pub(crate) signal: Option<String>,
    pub(crate) runtime_label: Option<String>,
    pub(crate) worktree_staleness: Option<u32>,
    pub(crate) health: AgentHealthSummary,
    pub(crate) health_summary: String,
    pub(crate) eta: String,
}

pub(crate) fn list_runtime_member_statuses(
    session: &str,
) -> Result<HashMap<String, RuntimeMemberStatus>> {
    let output = crate::tmux::run_tmux_with_timeout(
        [
            "list-panes",
            // -s: every window in the session (windows/focus layouts put
            // members in separate windows), not just the current one.
            "-s",
            "-t",
            session,
            "-F",
            "#{pane_id}\t#{@batty_role}\t#{@batty_status}\t#{pane_dead}",
        ],
        "list-panes runtime status",
        Some(session),
    )
    .with_context(|| format!("failed to list panes for session '{session}'"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("tmux list-panes runtime status failed: {stderr}");
    }

    let mut statuses = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(4, '\t');
        let Some(_pane_id) = parts.next() else {
            continue;
        };
        let Some(member_name) = parts.next() else {
            continue;
        };
        let Some(raw_status) = parts.next() else {
            continue;
        };
        let Some(pane_dead) = parts.next() else {
            continue;
        };
        if member_name.trim().is_empty() {
            continue;
        }

        statuses.insert(
            member_name.to_string(),
            summarize_runtime_member_status(raw_status, pane_dead == "1"),
        );
    }

    Ok(statuses)
}

pub(crate) fn summarize_runtime_member_status(
    raw_status: &str,
    pane_dead: bool,
) -> RuntimeMemberStatus {
    if pane_dead {
        return RuntimeMemberStatus {
            state: "crashed".to_string(),
            signal: None,
            label: Some("crashed".to_string()),
        };
    }

    let label = strip_tmux_style(raw_status);
    let normalized = label.to_ascii_lowercase();
    let has_paused_nudge = normalized.contains("nudge paused");
    let has_nudge_sent = normalized.contains("nudge sent");
    let has_waiting_nudge = normalized.contains("nudge") && !has_nudge_sent && !has_paused_nudge;
    let has_paused_standup = normalized.contains("standup paused");
    let has_standup = normalized.contains("standup") && !has_paused_standup;

    let state = if normalized.contains("crashed") {
        "crashed"
    } else if normalized.contains("working") {
        "working"
    } else if normalized.contains("done") || normalized.contains("completed") {
        "done"
    } else if normalized.contains("idle") {
        "idle"
    } else if label.is_empty() {
        "starting"
    } else {
        "unknown"
    };

    let mut signals = Vec::new();
    if has_paused_nudge {
        signals.push("nudge paused");
    } else if has_nudge_sent {
        signals.push("nudged");
    } else if has_waiting_nudge {
        signals.push("waiting for nudge");
    }
    if has_paused_standup {
        signals.push("standup paused");
    } else if has_standup {
        signals.push("standup");
    }
    let signal = (!signals.is_empty()).then(|| signals.join(", "));

    RuntimeMemberStatus {
        state: state.to_string(),
        signal,
        label: (!label.is_empty()).then_some(label),
    }
}

pub(crate) fn strip_tmux_style(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '#' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if next == ']' {
                    break;
                }
            }
            continue;
        }
        output.push(ch);
    }

    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_team_status_rows(
    members: &[MemberInstance],
    session_running: bool,
    runtime_statuses: &HashMap<String, RuntimeMemberStatus>,
    pending_inbox_counts: &HashMap<String, usize>,
    triage_backlog_counts: &HashMap<String, usize>,
    owned_task_buckets: &HashMap<String, OwnedTaskBuckets>,
    supervisory_pressures: &HashMap<String, SupervisoryPressureSnapshot>,
    branch_mismatches: &HashMap<String, String>,
    worktree_staleness: &HashMap<String, u32>,
    agent_health: &HashMap<String, AgentHealthSummary>,
) -> Vec<TeamStatusRow> {
    members
        .iter()
        .map(|member| {
            let runtime = runtime_statuses.get(&member.name);
            let pending_inbox = pending_inbox_counts.get(&member.name).copied().unwrap_or(0);
            let triage_backlog = triage_backlog_counts
                .get(&member.name)
                .copied()
                .unwrap_or(0);
            let owned_tasks = owned_task_buckets
                .get(&member.name)
                .cloned()
                .unwrap_or_default();
            let (state, signal, runtime_label) = if member.role_type == config::RoleType::User {
                ("user".to_string(), None, None)
            } else if !session_running {
                ("stopped".to_string(), None, None)
            } else if let Some(runtime) = runtime {
                (
                    runtime.state.clone(),
                    runtime.signal.clone(),
                    runtime.label.clone(),
                )
            } else {
                ("starting".to_string(), None, None)
            };

            let review_backlog = owned_tasks.review.len();
            let stale_review_backlog = owned_tasks.stale_review.len();
            let supervisory_pressure = supervisory_pressures
                .get(&member.name)
                .cloned()
                .unwrap_or_default();
            let state = if session_running && state == "idle" && review_backlog > 0 {
                "reviewing".to_string()
            } else if session_running && state == "idle" && triage_backlog > 0 {
                "triaging".to_string()
            } else {
                state
            };

            let health = agent_health.get(&member.name).cloned().unwrap_or_default();
            let signal = merge_status_signal(
                signal,
                branch_mismatches.get(&member.name).cloned(),
                health.stale_active_summary.clone(),
                health.stall_summary.clone(),
                &supervisory_pressure,
                stale_review_backlog,
            );
            let health_summary =
                format_agent_health_summary_for_role(&health, Some(member.role_type));
            let pending_inbox =
                if matches!(member.role_type, RoleType::Architect | RoleType::Manager)
                    && supervisory_pressure.actionable_count() > 0
                {
                    supervisory_pressure.actionable_count()
                } else {
                    pending_inbox
                };

            TeamStatusRow {
                name: member.name.clone(),
                role: member.role_name.clone(),
                role_type: format!("{:?}", member.role_type),
                agent: member.agent.clone(),
                reports_to: member.reports_to.clone(),
                state,
                pending_inbox,
                triage_backlog,
                active_owned_tasks: owned_tasks.active,
                review_owned_tasks: owned_tasks.review,
                signal,
                runtime_label,
                worktree_staleness: worktree_staleness.get(&member.name).copied(),
                health,
                health_summary,
                eta: "-".to_string(),
            }
        })
        .collect()
}
