use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::super::config::RoleType;
use super::super::daemon::MainSmokeState;
use super::super::events;
use super::super::hierarchy::MemberInstance;
use super::super::standup::MemberState;
use super::super::{daemon_state_path, now_unix, team_events_path};
use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct AgentHealthSummary {
    pub(crate) restart_count: u32,
    pub(crate) context_exhaustion_count: u32,
    pub(crate) proactive_handoff_count: u32,
    pub(crate) delivery_failure_count: u32,
    pub(crate) supervisory_digest_count: u32,
    pub(crate) dispatch_fallback_count: u32,
    pub(crate) dispatch_fallback_reason: Option<String>,
    pub(crate) stale_active_cleared_count: u32,
    pub(crate) stale_active_summary: Option<String>,
    pub(crate) task_elapsed_secs: Option<u64>,
    pub(crate) stall_summary: Option<String>,
    pub(crate) stall_reason: Option<String>,
    pub(crate) backend_health: crate::agent::BackendHealth,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(super) struct PersistedDaemonHealthState {
    #[serde(default)]
    pub(super) states: HashMap<String, MemberState>,
    #[serde(default)]
    pub(super) active_tasks: HashMap<String, u32>,
    #[serde(default)]
    pub(super) retry_counts: HashMap<String, u32>,
    #[serde(default)]
    pub(super) main_smoke_state: Option<MainSmokeState>,
    #[serde(default)]
    pub(super) running_tests: Vec<crate::team::test_job::RunningTestInfo>,
    #[serde(default)]
    pub(super) optional_subsystem_backoff: HashMap<String, u32>,
    #[serde(default)]
    pub(super) optional_subsystem_disabled_remaining_secs: HashMap<String, u64>,
}

pub(crate) fn agent_health_by_member(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, AgentHealthSummary> {
    let mut health_by_member = members
        .iter()
        .map(|member| (member.name.clone(), AgentHealthSummary::default()))
        .collect::<HashMap<_, _>>();

    let daemon_state = match load_persisted_daemon_health_state(&daemon_state_path(project_root)) {
        Ok(state) => state.unwrap_or_default(),
        Err(error) => {
            warn!(error = %error, "failed to load daemon health state");
            PersistedDaemonHealthState::default()
        }
    };

    for (member, retry_count) in &daemon_state.retry_counts {
        health_by_member
            .entry(member.clone())
            .or_default()
            .restart_count = health_by_member
            .get(member)
            .map(|health| health.restart_count.max(*retry_count))
            .unwrap_or(*retry_count);
    }

    let mut restart_events = HashMap::<String, u32>::new();
    let mut latest_assignment_ts = HashMap::<String, u64>::new();
    let mut latest_assignment_ts_by_task = HashMap::<(String, u32), u64>::new();
    // Track the latest daemon_started timestamp so stall events from prior
    // daemon sessions don't leak into the current status display. Without
    // this, a stall_detected event from 2 hours ago (before the last
    // restart) appears as a live "stalled after 2h" signal on a freshly
    // restarted member.
    let mut latest_daemon_started_ts: u64 = 0;
    match events::read_events(&team_events_path(project_root)) {
        Ok(events) => {
            for event in events {
                // Session lifecycle events don't carry a role but are
                // still meaningful — process them before the role guard.
                if event.event == "daemon_started" {
                    if event.ts > latest_daemon_started_ts {
                        latest_daemon_started_ts = event.ts;
                        // Clear stall state for every member so stall events
                        // from prior sessions don't affect the current
                        // display.
                        for health in health_by_member.values_mut() {
                            health.record_supervisory_stall(None, None);
                        }
                    }
                    continue;
                }

                let Some(role) = event.role.as_deref() else {
                    continue;
                };

                match event.event.as_str() {
                    "agent_restarted" => {
                        *restart_events.entry(role.to_string()).or_insert(0) += 1;
                        if let Some(restart_count) = event.restart_count {
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .restart_count = health_by_member
                                .get(role)
                                .map(|health| health.restart_count.max(restart_count))
                                .unwrap_or(restart_count);
                        }
                        if event.reason.as_deref() == Some("context_pressure") {
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .proactive_handoff_count += 1;
                        }
                    }
                    "context_exhausted" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .context_exhaustion_count += 1;
                    }
                    "delivery_failed" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .delivery_failure_count += 1;
                    }
                    "supervisory_digest_emitted" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .record_supervisory_digest();
                    }
                    "dispatch_fallback_used" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .record_dispatch_fallback(event.reason.as_deref());
                    }
                    "state_reconciliation" => {
                        health_by_member
                            .entry(role.to_string())
                            .or_default()
                            .record_stale_active_clear(
                                event.task.as_deref(),
                                event.reason.as_deref(),
                            );
                    }
                    "stall_detected" => {
                        let role_type = members
                            .iter()
                            .find(|member| member.name == role)
                            .map(|member| member.role_type);
                        let is_supervisory = event
                            .task
                            .as_deref()
                            .is_some_and(|task| task.starts_with("supervisory::"))
                            || event
                                .reason
                                .as_deref()
                                .is_some_and(is_supervisory_stall_reason)
                            || role_type.is_some_and(|role_type| {
                                matches!(role_type, RoleType::Architect | RoleType::Manager)
                            });
                        if is_supervisory {
                            let fallback_summary = fallback_supervisory_stall_summary(
                                event.reason.as_deref(),
                                event.uptime_secs,
                                role_type,
                            );
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .record_supervisory_stall(
                                    event.reason.as_deref(),
                                    event.details.as_deref().or(fallback_summary.as_deref()),
                                );
                        }
                    }
                    "task_assigned" => {
                        latest_assignment_ts.insert(role.to_string(), event.ts);
                        if let Some(task_id) =
                            event.task.as_deref().and_then(parse_assigned_task_id)
                        {
                            latest_assignment_ts_by_task
                                .insert((role.to_string(), task_id), event.ts);
                        }
                    }
                    "health_changed" => {
                        // Parse the latest backend health from the transition reason.
                        // Format is "prev→new", e.g. "healthy→unreachable".
                        if let Some(reason) = event.reason.as_deref() {
                            let new_state = reason.split('→').next_back().unwrap_or("healthy");
                            let health_val = match new_state {
                                "degraded" => crate::agent::BackendHealth::Degraded,
                                "unreachable" => crate::agent::BackendHealth::Unreachable,
                                _ => crate::agent::BackendHealth::Healthy,
                            };
                            health_by_member
                                .entry(role.to_string())
                                .or_default()
                                .backend_health = health_val;
                        }
                    }
                    _ => {}
                }
            }
        }
        Err(error) => {
            warn!(error = %error, "failed to read team events for status health summary");
        }
    }

    for (member, event_count) in restart_events {
        let health = health_by_member.entry(member).or_default();
        health.restart_count = health.restart_count.max(event_count);
    }

    let now = now_unix();
    for (member, task_id) in daemon_state.active_tasks {
        let assigned_ts = latest_assignment_ts_by_task
            .get(&(member.clone(), task_id))
            .copied()
            .or_else(|| latest_assignment_ts.get(&member).copied());
        if let Some(assigned_ts) = assigned_ts {
            health_by_member
                .entry(member)
                .or_default()
                .task_elapsed_secs = Some(now.saturating_sub(assigned_ts));
        }
    }

    health_by_member
}

pub(super) fn load_persisted_daemon_health_state(
    path: &Path,
) -> Result<Option<PersistedDaemonHealthState>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let state = serde_json::from_str::<PersistedDaemonHealthState>(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(state))
}

pub(super) fn parse_assigned_task_id(task: &str) -> Option<u32> {
    let trimmed = task.trim();
    trimmed
        .parse::<u32>()
        .ok()
        .or_else(|| {
            trimmed
                .split("Task #")
                .nth(1)
                .and_then(parse_leading_task_id)
        })
        .or_else(|| {
            trimmed
                .find('#')
                .and_then(|idx| parse_leading_task_id(&trimmed[idx + 1..]))
        })
}

fn stale_active_clear_summary(task: Option<&str>, reason: Option<&str>) -> Option<String> {
    let label = match reason {
        Some("clear_done") => "done",
        Some("clear_archived") => "archived",
        Some("clear_missing") => "missing",
        Some("clear_unassigned") => "unassigned",
        Some("release_review") => "review",
        Some("release_blocked") => "blocked",
        _ => return None,
    };
    let task = task.and_then(parse_assigned_task_id);
    Some(match task {
        Some(task_id) => format!("cleared stale active #{task_id} ({label})"),
        None => format!("cleared stale active ({label})"),
    })
}

fn parse_leading_task_id(value: &str) -> Option<u32> {
    let digits = value
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

impl AgentHealthSummary {
    pub(crate) fn record_supervisory_digest(&mut self) {
        self.supervisory_digest_count += 1;
    }

    pub(crate) fn record_dispatch_fallback(&mut self, reason: Option<&str>) {
        self.dispatch_fallback_count += 1;
        self.dispatch_fallback_reason = reason.map(str::to_string);
    }

    pub(crate) fn record_stale_active_clear(&mut self, task: Option<&str>, reason: Option<&str>) {
        let Some(summary) = stale_active_clear_summary(task, reason) else {
            return;
        };
        self.stale_active_cleared_count += 1;
        self.stale_active_summary = Some(summary);
    }

    pub(crate) fn record_supervisory_stall(&mut self, reason: Option<&str>, summary: Option<&str>) {
        self.stall_reason = reason.map(str::to_string);
        self.stall_summary = summary.map(str::to_string);
    }

    pub(crate) fn has_supervisory_warning(&self) -> bool {
        self.stall_reason.is_some() || self.stall_summary.is_some()
    }

    pub(crate) fn has_operator_warning(&self) -> bool {
        self.restart_count > 0
            || self.context_exhaustion_count > 0
            || self.proactive_handoff_count > 0
            || self.delivery_failure_count > 0
            || self.stale_active_cleared_count > 0
            || self.has_supervisory_warning()
            || !self.backend_health.is_healthy()
    }

    pub(crate) fn supervisory_status_token_for_role(
        &self,
        role_type: Option<RoleType>,
    ) -> Option<String> {
        if !self.has_supervisory_warning() {
            return None;
        }

        let role_label = role_type.and_then(supervisory_role_label).or_else(|| {
            self.stall_reason
                .as_deref()
                .and_then(supervisory_role_label_from_reason)
        });
        Some(match self.stall_reason.as_deref() {
            Some(reason) => supervisory_status_token(reason, role_label),
            None => role_label
                .map(|label| format!("stall:{label}"))
                .unwrap_or_else(|| "stall".to_string()),
        })
    }
}
