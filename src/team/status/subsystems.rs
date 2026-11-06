use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Context;
use serde::Serialize;
use tracing::warn;

use super::super::daemon_mgmt::{
    PersistedWatchdogState, daemon_child_pid_path, daemon_log_path, process_exists,
    watchdog_pid_path, watchdog_state_path,
};
use super::super::events;
use super::super::{daemon_state_path, now_unix, team_events_path};
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OptionalSubsystemStatus {
    pub(crate) name: String,
    pub(crate) state: String,
    pub(crate) recent_errors: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) disabled_remaining_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backoff_stage: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TeamStatusHealth {
    pub(crate) session_running: bool,
    pub(crate) paused: bool,
    pub(crate) member_count: usize,
    pub(crate) active_member_count: usize,
    pub(crate) pending_inbox_count: usize,
    pub(crate) triage_backlog_count: usize,
    pub(crate) unhealthy_members: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct WatchdogStatus {
    pub(crate) state: String,
    pub(crate) restart_count: u32,
    pub(crate) current_backoff_secs: Option<u64>,
    pub(crate) last_exit_category: Option<String>,
    pub(crate) last_exit_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watchdog_pid: Option<u32>,
    pub(crate) watchdog_pid_live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_pid: Option<u32>,
    pub(crate) daemon_pid_live: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watchdog_state_updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) watchdog_state_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_state_updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_state_age_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_log_updated_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) daemon_log_age_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) orphan_codex_execs: Vec<OrphanProcessStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OrphanProcessStatus {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) command: String,
}

pub(crate) fn load_optional_subsystem_statuses(
    project_root: &Path,
) -> Vec<OptionalSubsystemStatus> {
    let daemon_state = load_persisted_daemon_health_state(&daemon_state_path(project_root))
        .ok()
        .flatten()
        .unwrap_or_default();
    let recent_errors = recent_optional_subsystem_errors(project_root);

    crate::team::daemon::optional_subsystem_names()
        .into_iter()
        .map(|name| {
            let disabled_remaining_secs = daemon_state
                .optional_subsystem_disabled_remaining_secs
                .get(name)
                .copied()
                .filter(|secs| *secs > 0);
            let recent_errors = recent_errors.get(name).copied().unwrap_or(0);
            let state = if disabled_remaining_secs.is_some() {
                "disabled"
            } else if recent_errors > 0 {
                "degraded"
            } else {
                "healthy"
            };
            OptionalSubsystemStatus {
                name: name.to_string(),
                state: state.to_string(),
                recent_errors,
                disabled_remaining_secs,
                backoff_stage: daemon_state.optional_subsystem_backoff.get(name).copied(),
                last_error: latest_optional_subsystem_error(project_root, name),
            }
        })
        .collect()
}

pub(crate) fn format_optional_subsystem_statuses(statuses: &[OptionalSubsystemStatus]) -> String {
    let mut lines = vec![
        "Optional Subsystems".to_string(),
        format!(
            "{:<12} {:<10} {:<12} {:<12} {}",
            "NAME", "STATE", "ERRORS/10M", "RETRY", "LAST ERROR"
        ),
    ];

    for status in statuses {
        let retry = status
            .disabled_remaining_secs
            .map(format_health_duration)
            .unwrap_or_else(|| "-".to_string());
        let last_error = status.last_error.as_deref().unwrap_or("-");
        lines.push(format!(
            "{:<12} {:<10} {:<12} {:<12} {}",
            status.name, status.state, status.recent_errors, retry, last_error
        ));
    }

    lines.join("\n")
}

fn recent_optional_subsystem_errors(project_root: &Path) -> HashMap<&'static str, usize> {
    let cutoff = now_unix().saturating_sub(600);
    let mut counts = HashMap::new();
    let Ok(events) = events::read_events(&team_events_path(project_root)) else {
        return counts;
    };

    for event in events {
        if event.event != "loop_step_error" || event.ts < cutoff {
            continue;
        }
        let Some(subsystem) = event
            .step
            .as_deref()
            .and_then(crate::team::daemon::optional_subsystem_for_step)
        else {
            continue;
        };
        *counts.entry(subsystem).or_insert(0) += 1;
    }

    counts
}

fn latest_optional_subsystem_error(project_root: &Path, subsystem: &str) -> Option<String> {
    events::read_events(&team_events_path(project_root))
        .ok()?
        .into_iter()
        .rev()
        .find(|event| {
            event.event == "loop_step_error"
                && event
                    .step
                    .as_deref()
                    .and_then(crate::team::daemon::optional_subsystem_for_step)
                    == Some(subsystem)
        })
        .and_then(|event| event.error)
}

pub(crate) fn load_watchdog_status(project_root: &Path, session_running: bool) -> WatchdogStatus {
    let watchdog_state = watchdog_state_path(project_root);
    let persisted = if !watchdog_state.exists() {
        PersistedWatchdogState::default()
    } else {
        match std::fs::read_to_string(&watchdog_state)
            .with_context(|| format!("failed to read {}", watchdog_state.display()))
            .and_then(|content| {
                serde_json::from_str::<PersistedWatchdogState>(&content)
                    .with_context(|| format!("failed to parse {}", watchdog_state.display()))
            }) {
            Ok(state) => state,
            Err(error) => {
                warn!(error = %error, "failed to load watchdog state");
                PersistedWatchdogState::default()
            }
        }
    };

    let watchdog_pid = read_status_pid(&watchdog_pid_path(project_root));
    let watchdog_pid_live = watchdog_pid.is_some_and(process_exists);
    let daemon_pid = persisted
        .child_pid
        .or_else(|| read_status_pid(&daemon_child_pid_path(project_root)));
    let daemon_pid_live = daemon_pid.is_some_and(process_exists);
    let now = SystemTime::now();
    let watchdog_state_updated_at = file_modified_unix_secs(&watchdog_state);
    let watchdog_state_age_secs = file_age_secs(&watchdog_state, now);
    let daemon_state = daemon_state_path(project_root);
    let daemon_state_updated_at = file_modified_unix_secs(&daemon_state);
    let daemon_state_age_secs = file_age_secs(&daemon_state, now);
    let daemon_log = daemon_log_path(project_root);
    let daemon_log_updated_at = file_modified_unix_secs(&daemon_log);
    let daemon_log_age_secs = file_age_secs(&daemon_log, now);

    let state = if !session_running {
        "stopped".to_string()
    } else if !watchdog_pid_live {
        "offline".to_string()
    } else if persisted.circuit_breaker_tripped {
        "circuit-open".to_string()
    } else if persisted.current_backoff_secs.is_some() {
        "restarting".to_string()
    } else if daemon_pid_live {
        "running".to_string()
    } else {
        "degraded".to_string()
    };
    let orphan_codex_execs = if session_running
        && matches!(
            state.as_str(),
            "offline" | "degraded" | "circuit-open" | "restarting"
        ) {
        load_codex_exec_process_statuses()
    } else {
        Vec::new()
    };

    WatchdogStatus {
        state,
        restart_count: persisted.restart_count,
        current_backoff_secs: persisted.current_backoff_secs,
        last_exit_category: persisted.last_exit_category,
        last_exit_reason: persisted.last_exit_reason,
        watchdog_pid,
        watchdog_pid_live,
        daemon_pid,
        daemon_pid_live,
        watchdog_state_updated_at,
        watchdog_state_age_secs,
        daemon_state_updated_at,
        daemon_state_age_secs,
        daemon_log_updated_at,
        daemon_log_age_secs,
        orphan_codex_execs,
    }
}

pub(crate) fn format_watchdog_summary(watchdog: &WatchdogStatus) -> String {
    let mut parts = vec![
        watchdog.state.clone(),
        format!("r{}", watchdog.restart_count),
    ];
    if let Some(backoff_secs) = watchdog.current_backoff_secs {
        parts.push(format!("backoff={}s", backoff_secs));
    }
    if let Some(pid) = watchdog.watchdog_pid {
        parts.push(format!(
            "watchdog-pid={pid}:{}",
            if watchdog.watchdog_pid_live {
                "live"
            } else {
                "dead"
            }
        ));
    } else if watchdog.state != "stopped" {
        parts.push("watchdog-pid=missing".to_string());
    }
    if let Some(pid) = watchdog.daemon_pid {
        parts.push(format!(
            "daemon-pid={pid}:{}",
            if watchdog.daemon_pid_live {
                "live"
            } else {
                "dead"
            }
        ));
    } else if matches!(watchdog.state.as_str(), "running" | "degraded" | "offline") {
        parts.push("daemon-pid=missing".to_string());
    }
    if let Some(age_secs) = watchdog.daemon_state_age_secs {
        parts.push(format!("state-age={}", format_health_duration(age_secs)));
    }
    if let Some(updated_at) = watchdog.daemon_state_updated_at {
        parts.push(format!("state-updated={updated_at}"));
    }
    if let Some(age_secs) = watchdog.daemon_log_age_secs {
        parts.push(format!("log-age={}", format_health_duration(age_secs)));
    }
    if let Some(updated_at) = watchdog.daemon_log_updated_at {
        parts.push(format!("log-updated={updated_at}"));
    }
    if !watchdog.orphan_codex_execs.is_empty() {
        let pids = watchdog
            .orphan_codex_execs
            .iter()
            .take(5)
            .map(|process| format!("{}<-{}", process.pid, process.ppid))
            .collect::<Vec<_>>()
            .join(",");
        let suffix = if watchdog.orphan_codex_execs.len() > 5 {
            format!("+{}", watchdog.orphan_codex_execs.len() - 5)
        } else {
            String::new()
        };
        parts.push(format!(
            "orphan-codex-exec={} [{}{}]",
            watchdog.orphan_codex_execs.len(),
            pids,
            suffix
        ));
    }
    if let Some(category) = &watchdog.last_exit_category {
        if category != crate::team::daemon_mgmt::DAEMON_EXIT_CATEGORY_UNKNOWN {
            parts.push(category.clone());
        }
    }
    if let Some(reason) = &watchdog.last_exit_reason {
        parts.push(reason.clone());
    }
    parts.join(" | ")
}

fn load_codex_exec_process_statuses() -> Vec<OrphanProcessStatus> {
    match super::super::process_tree::codex_exec_processes() {
        Ok(processes) => processes
            .into_iter()
            .map(|process| OrphanProcessStatus {
                pid: process.pid,
                ppid: process.ppid,
                command: process.command,
            })
            .collect(),
        Err(error) => {
            warn!(error = %error, "failed to inspect codex exec processes for status");
            Vec::new()
        }
    }
}

fn read_status_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

fn file_modified_unix_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

pub(crate) fn build_team_status_health(
    rows: &[TeamStatusRow],
    session_running: bool,
    paused: bool,
) -> TeamStatusHealth {
    let member_rows: Vec<&TeamStatusRow> =
        rows.iter().filter(|row| row.role_type != "User").collect();
    let mut unhealthy_members = member_rows
        .iter()
        .filter(|row| row.health.has_operator_warning())
        .map(|row| row.name.clone())
        .collect::<Vec<_>>();
    unhealthy_members.sort();

    TeamStatusHealth {
        session_running,
        paused,
        member_count: member_rows.len(),
        active_member_count: member_rows
            .iter()
            .filter(|row| matches!(row.state.as_str(), "working" | "triaging" | "reviewing"))
            .count(),
        pending_inbox_count: member_rows.iter().map(|row| row.pending_inbox).sum(),
        triage_backlog_count: member_rows.iter().map(|row| row.triage_backlog).sum(),
        unhealthy_members,
    }
}
