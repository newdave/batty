use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use tracing::warn;

use crate::task;

use super::super::config::RoleType;
use super::super::daemon::NudgeSchedule;
use super::super::hierarchy::MemberInstance;
use super::super::inbox;
use super::super::pause_marker_path;
use super::super::standup::MemberState;
use super::*;

pub(crate) struct PaneStatusLabelUpdateContext<'a, F>
where
    F: Fn(&str) -> Option<Duration>,
{
    pub(crate) project_root: &'a Path,
    /// Board tasks from the daemon's per-tick snapshot.
    pub(crate) board_tasks: &'a [task::Task],
    pub(crate) members: &'a [MemberInstance],
    pub(crate) pane_map: &'a HashMap<String, String>,
    pub(crate) states: &'a HashMap<String, MemberState>,
    pub(crate) nudges: &'a HashMap<String, NudgeSchedule>,
    pub(crate) last_standup: &'a HashMap<String, Instant>,
    pub(crate) paused_standups: &'a HashSet<String>,
    pub(crate) standup_interval_for_member: F,
}

pub(crate) fn update_pane_status_labels<F>(context: PaneStatusLabelUpdateContext<'_, F>)
where
    F: Fn(&str) -> Option<Duration>,
{
    let PaneStatusLabelUpdateContext {
        project_root,
        board_tasks,
        members,
        pane_map,
        states,
        nudges,
        last_standup,
        paused_standups,
        standup_interval_for_member,
    } = context;
    let globally_paused = pause_marker_path(project_root).exists();
    let inbox_root = inbox::inboxes_root(project_root);
    let direct_reports = direct_reports_by_member(members);
    let owned_task_buckets = owned_task_buckets_from_tasks(project_root, members, board_tasks);

    for member in members {
        if member.role_type == RoleType::User {
            continue;
        }
        let Some(pane_id) = pane_map.get(&member.name) else {
            continue;
        };

        let state = states
            .get(&member.name)
            .copied()
            .unwrap_or(MemberState::Idle);

        let pending_inbox = match inbox::pending_message_count(&inbox_root, &member.name) {
            Ok(count) => count,
            Err(error) => {
                warn!(member = %member.name, error = %error, "failed to count pending inbox messages");
                0
            }
        };
        let triage_backlog = match direct_reports.get(&member.name) {
            Some(reports) => {
                match delivered_direct_report_triage_count(&inbox_root, &member.name, reports) {
                    Ok(count) => count,
                    Err(error) => {
                        warn!(member = %member.name, error = %error, "failed to compute triage backlog");
                        0
                    }
                }
            }
            None => 0,
        };
        let member_owned_tasks = owned_task_buckets
            .get(&member.name)
            .cloned()
            .unwrap_or_default();

        let label = if globally_paused {
            compose_pane_status_label(PaneStatusLabelArgs {
                state,
                pending_inbox,
                triage_backlog,
                active_task_ids: &member_owned_tasks.active,
                review_task_ids: &member_owned_tasks.review,
                globally_paused: true,
                nudge_status: "",
                standup_status: "",
            })
        } else {
            let nudge_str = format_nudge_status(nudges.get(&member.name));
            let standup_str = standup_interval_for_member(&member.name)
                .map(|standup_interval| {
                    format_standup_status(
                        last_standup.get(&member.name).copied(),
                        standup_interval,
                        paused_standups.contains(&member.name),
                    )
                })
                .unwrap_or_default();
            compose_pane_status_label(PaneStatusLabelArgs {
                state,
                pending_inbox,
                triage_backlog,
                active_task_ids: &member_owned_tasks.active,
                review_task_ids: &member_owned_tasks.review,
                globally_paused: false,
                nudge_status: &nudge_str,
                standup_status: &standup_str,
            })
        };

        let _ = crate::tmux::run_tmux_with_timeout(
            ["set-option", "-p", "-t", pane_id, "@batty_status", &label],
            "set-option @batty_status",
            Some(pane_id),
        );
    }
}

pub(crate) fn format_nudge_status(schedule: Option<&NudgeSchedule>) -> String {
    let Some(schedule) = schedule else {
        return String::new();
    };

    if schedule.fired_this_idle {
        return " #[fg=magenta]nudge sent#[default]".to_string();
    }

    if schedule.paused {
        return " #[fg=244]nudge paused#[default]".to_string();
    }

    let Some(idle_since) = schedule.idle_since else {
        return String::new();
    };

    let elapsed = idle_since.elapsed();
    if elapsed < schedule.interval {
        let remaining = schedule.interval - elapsed;
        let mins = remaining.as_secs() / 60;
        let secs = remaining.as_secs() % 60;
        format!(" #[fg=magenta]nudge {mins}:{secs:02}#[default]")
    } else {
        " #[fg=magenta]nudge now#[default]".to_string()
    }
}

fn format_inbox_status(pending_count: usize) -> String {
    if pending_count == 0 {
        " #[fg=244]inbox 0#[default]".to_string()
    } else {
        format!(" #[fg=colour214,bold]inbox {pending_count}#[default]")
    }
}

fn format_active_task_status(active_task_ids: &[u32]) -> String {
    match active_task_ids {
        [] => String::new(),
        [task_id] => format!(" #[fg=green,bold]task {task_id}#[default]"),
        _ => format!(" #[fg=green,bold]tasks {}#[default]", active_task_ids.len()),
    }
}

fn format_review_task_status(review_task_ids: &[u32]) -> String {
    match review_task_ids {
        [] => String::new(),
        [task_id] => format!(" #[fg=blue,bold]review {task_id}#[default]"),
        _ => format!(" #[fg=blue,bold]review {}#[default]", review_task_ids.len()),
    }
}

pub(crate) struct PaneStatusLabelArgs<'a> {
    pub(crate) state: MemberState,
    pub(crate) pending_inbox: usize,
    pub(crate) triage_backlog: usize,
    pub(crate) active_task_ids: &'a [u32],
    pub(crate) review_task_ids: &'a [u32],
    pub(crate) globally_paused: bool,
    pub(crate) nudge_status: &'a str,
    pub(crate) standup_status: &'a str,
}

pub(crate) fn compose_pane_status_label(args: PaneStatusLabelArgs<'_>) -> String {
    let PaneStatusLabelArgs {
        state,
        pending_inbox,
        triage_backlog,
        active_task_ids,
        review_task_ids,
        globally_paused,
        nudge_status,
        standup_status,
    } = args;
    let state_str = match state {
        MemberState::Idle => "#[fg=yellow]idle#[default]",
        MemberState::Working => "#[fg=cyan]working#[default]",
    };
    let inbox_str = format_inbox_status(pending_inbox);
    let triage_str = if triage_backlog > 0 {
        format!(" #[fg=red,bold]triage {triage_backlog}#[default]")
    } else {
        String::new()
    };
    let active_task_str = format_active_task_status(active_task_ids);
    let review_task_str = format_review_task_status(review_task_ids);

    if globally_paused {
        return format!(
            "{state_str}{inbox_str}{triage_str}{active_task_str}{review_task_str} #[fg=red]PAUSED#[default]"
        );
    }

    format!(
        "{state_str}{inbox_str}{triage_str}{active_task_str}{review_task_str}{nudge_status}{standup_status}"
    )
}

pub(crate) fn format_standup_status(
    last_standup: Option<Instant>,
    interval: Duration,
    paused: bool,
) -> String {
    if paused {
        return " #[fg=244]standup paused#[default]".to_string();
    }

    let Some(last_standup) = last_standup else {
        return String::new();
    };

    let elapsed = last_standup.elapsed();
    if elapsed < interval {
        let remaining = interval - elapsed;
        let mins = remaining.as_secs() / 60;
        let secs = remaining.as_secs() % 60;
        format!(" #[fg=blue]standup {mins}:{secs:02}#[default]")
    } else {
        " #[fg=blue]standup now#[default]".to_string()
    }
}
