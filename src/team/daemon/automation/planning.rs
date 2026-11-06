//! Pipeline starvation detection, tact checks, and the planning cycle.

use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::*;

pub(super) fn normalized_context_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("```") {
        return None;
    }

    let trimmed = trimmed.trim_start_matches('#').trim();
    let trimmed = trimmed.trim_start_matches('-').trim();
    let trimmed = trimmed.trim_start_matches('*').trim();
    let trimmed = trimmed
        .strip_prefix(|ch: char| ch.is_ascii_digit())
        .and_then(|rest| rest.strip_prefix('.'))
        .map(str::trim)
        .unwrap_or(trimmed);

    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn collect_context_lines(path: &std::path::Path, limit: usize) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|content| {
            content
                .lines()
                .filter_map(normalized_context_line)
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}

fn planning_prompt_context(project_root: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let planning_dir = project_root.join("planning");
    let roadmap = collect_context_lines(&planning_dir.join("roadmap.md"), 6);

    let goals = [
        planning_dir.join("architecture.md"),
        planning_dir.join("dev-philosophy.md"),
        project_root.join("README.md"),
    ]
    .into_iter()
    .find_map(|path| {
        let lines = collect_context_lines(&path, 6);
        if lines.is_empty() { None } else { Some(lines) }
    })
    .unwrap_or_default();

    (roadmap, goals)
}

fn planning_priority_rank(priority: &str) -> u8 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn top_blocked_task_summaries(tasks: &[crate::task::Task], limit: usize) -> Vec<String> {
    let mut blocked = tasks
        .iter()
        .filter(|task| task.status == "blocked" || task.blocked.is_some())
        .collect::<Vec<_>>();
    blocked.sort_by_key(|task| (planning_priority_rank(&task.priority), task.id));
    blocked
        .into_iter()
        .take(limit)
        .map(|task| {
            let reason = task_blocker_reason(task, tasks).unwrap_or_else(|| "blocked".to_string());
            format!("#{} {}: {}", task.id, task.title, reason)
        })
        .collect()
}

fn top_review_drain_task_summaries(tasks: &[crate::task::Task], limit: usize) -> Vec<String> {
    let held_tasks = top_blocked_task_summaries(tasks, 5);
    let held_summary = if held_tasks.is_empty() {
        "review backlog is holding dispatchable follow-up work".to_string()
    } else {
        held_tasks.join("; ")
    };
    let mut review_tasks = tasks
        .iter()
        .filter(|task| task.status == "review")
        .filter(|task| !crate::team::status::is_review_task_explicitly_blocked(task))
        .collect::<Vec<_>>();
    review_tasks.sort_by_key(|task| (planning_priority_rank(&task.priority), task.id));

    review_tasks
        .into_iter()
        .take(limit)
        .map(|task| {
            let review_owner = task.review_owner.as_deref().unwrap_or("unassigned");
            let branch = task.branch.as_deref().unwrap_or("missing");
            let commit = task.commit.as_deref().unwrap_or("missing");
            let next_action = task.next_action.as_deref().unwrap_or("review disposition");
            let artifacts = if task.artifacts.is_empty() {
                "none".to_string()
            } else {
                task.artifacts.join(", ")
            };
            format!(
                "#{} {}: review_owner={review_owner}; branch={branch}; commit={commit}; artifacts={artifacts}; next_action={next_action}; gates held tasks: {held_summary}",
                task.id, task.title
            )
        })
        .collect()
}

fn planning_board_state_label(
    tasks: &[crate::task::Task],
    dispatchable_task_count: usize,
    actionable_review_count: usize,
    blocked_count: usize,
) -> &'static str {
    if tasks
        .iter()
        .all(|task| matches!(task.status.as_str(), "done" | "archived"))
    {
        "empty-board"
    } else if dispatchable_task_count > 0 {
        "runnable-board"
    } else if actionable_review_count > 0 {
        "review-backlog-gated"
    } else if blocked_count > 0 {
        "blocked-only-board"
    } else {
        "active-board"
    }
}

impl TeamDaemon {
    pub(in crate::team::daemon) fn maybe_detect_pipeline_starvation(&mut self) -> Result<()> {
        let Some(threshold) = self
            .config
            .team_config
            .workflow_policy
            .pipeline_starvation_threshold
        else {
            self.scheduler.pipeline_starvation_fired = false;
            return Ok(());
        };

        // Already fired — stay suppressed until condition fully clears
        if self.scheduler.pipeline_starvation_fired {
            // Only reset when enough unclaimed work exists for all idle engineers
            let all_tasks = self.board_tasks()?;
            let unclaimed_todo = all_tasks
                .iter()
                .filter(|t| matches!(t.status.as_str(), "todo" | "backlog"))
                .filter(|t| t.claimed_by.is_none())
                .count();
            let truly_idle = self.truly_idle_engineer_count(&all_tasks);
            if truly_idle == 0 || unclaimed_todo > truly_idle {
                self.scheduler.pipeline_starvation_fired = false;
                self.scheduler.pipeline_starvation_last_fired = None;
            } else {
                return Ok(());
            }
        }

        // Hard cooldown: never fire more than once per 5 minutes
        pub(super) const STARVATION_COOLDOWN: Duration = Duration::from_secs(300);
        if let Some(last) = self.scheduler.pipeline_starvation_last_fired {
            if last.elapsed() < STARVATION_COOLDOWN {
                return Ok(());
            }
        }

        // Suppress if manager has been actively working for less than 10 minutes.
        // Previously this was an unconditional suppression, causing permanent
        // deadlock when the shim state classifier got stuck on "working".
        const MANAGER_WORKING_GRACE: Duration = Duration::from_secs(600);
        let manager_recently_working = self.config.members.iter().any(|m| {
            m.role_type == RoleType::Manager
                && self.states.get(&m.name) == Some(&MemberState::Working)
                && self
                    .shim_handles
                    .get(&m.name)
                    .map(|handle| {
                        handle.secs_since_state_change() < MANAGER_WORKING_GRACE.as_secs()
                    })
                    // For non-shim agents, check if idle_started_at is absent
                    // (meaning they transitioned to working recently)
                    .unwrap_or_else(|| !self.idle_started_at.contains_key(&m.name))
        });
        if manager_recently_working {
            return Ok(());
        }

        let all_tasks = self.board_tasks()?;
        let idle_count = self.truly_idle_engineer_count(&all_tasks);
        if idle_count == 0 {
            return Ok(());
        }

        let todo_count = all_tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "backlog"))
            .filter(|task| task.claimed_by.is_none())
            .count();

        let deficit = idle_count.saturating_sub(todo_count);
        if todo_count >= idle_count || deficit < threshold {
            return Ok(());
        }

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let architects: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .collect();
        if architects.is_empty() {
            return Ok(());
        }

        let message =
            format!("Pipeline running dry: {idle_count} idle engineers, {todo_count} todo tasks.");
        for architect in &architects {
            let visible_sender = self.automation_sender_for(architect);
            let inbox_msg = inbox::InboxMessage::new_send(&visible_sender, architect, &message);
            inbox::deliver_to_inbox(&inbox_root, &inbox_msg)?;
        }
        self.emit_event(TeamEvent::pipeline_starvation_detected(
            idle_count, todo_count,
        ));
        self.scheduler.pipeline_starvation_fired = true;
        self.scheduler.pipeline_starvation_last_fired = Some(Instant::now());
        Ok(())
    }

    pub(in crate::team::daemon) fn tact_check(&mut self) -> Result<()> {
        let base_cooldown = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .planning_cycle_cooldown_secs,
        );
        // #681: when recent planning cycles have produced zero new tasks,
        // extend the cooldown linearly (1x → 6x) so a stuck board doesn't
        // burn orchestrator tokens with empty cycles every few minutes.
        // Resets to 1x as soon as a cycle creates any tasks.
        let backoff_multiplier = 1 + self.scheduler.planning_cycle_consecutive_empty.min(5);
        let cooldown = base_cooldown * backoff_multiplier;

        let Some(architect) = self
            .config
            .members
            .iter()
            .find(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
        else {
            return Ok(());
        };

        // Cheap cooldown/active gates first: planning_cycle_ready would reject
        // these anyway, so skip parsing the board.
        if self.scheduler.planning_cycle_active
            || self
                .scheduler
                .planning_cycle_last_fired
                .is_some_and(|last| last.elapsed() < cooldown)
        {
            return Ok(());
        }

        let board_tasks = self.board_tasks().unwrap_or_default();
        let idle_engineer_count = self.truly_idle_engineer_count(&board_tasks);
        let dispatchable_task_count =
            crate::team::tact::dispatchable_task_count_from(&board_tasks, &self.config.members);
        if !crate::team::tact::should_trigger(idle_engineer_count, dispatchable_task_count) {
            return Ok(());
        }
        if !crate::team::tact::planning_cycle_ready(
            self.scheduler.planning_cycle_active,
            self.scheduler.planning_cycle_last_fired,
            cooldown,
            idle_engineer_count,
            dispatchable_task_count,
        ) {
            return Ok(());
        }
        let actionable_review_count = board_tasks
            .iter()
            .filter(|task| task.status == "review")
            .filter(|task| !crate::team::status::is_review_task_explicitly_blocked(task))
            .count();
        let blocked_count = board_tasks
            .iter()
            .filter(|task| task.status == "blocked" || task.blocked.is_some())
            .count();
        let blocked_task_summaries = if dispatchable_task_count == 0 {
            top_blocked_task_summaries(&board_tasks, 5)
        } else {
            Vec::new()
        };
        let planning_state = planning_board_state_label(
            &board_tasks,
            dispatchable_task_count,
            actionable_review_count,
            blocked_count,
        );
        let review_drain_task_summaries = if planning_state == "review-backlog-gated"
            && dispatchable_task_count == 0
            && actionable_review_count > 0
        {
            top_review_drain_task_summaries(&board_tasks, 1)
        } else {
            Vec::new()
        };
        let implementation_work_summary = crate::team::tact::parser::implementation_work_summary(
            dispatchable_task_count,
            actionable_review_count,
        );
        let board_summary = format!(
            "todo={}, backlog={}, in-progress={}, review={}, actionable_review={}, blocked={}, done={}, idle_engineers={}, dispatchable_tasks={}, planning_state={}, implementation_work={}",
            board_tasks
                .iter()
                .filter(|task| task.status == "todo")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "backlog")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "in-progress")
                .count(),
            board_tasks
                .iter()
                .filter(|task| task.status == "review")
                .count(),
            actionable_review_count,
            blocked_count,
            board_tasks
                .iter()
                .filter(|task| task.status == "done")
                .count(),
            idle_engineer_count,
            dispatchable_task_count,
            planning_state,
            implementation_work_summary
        );
        let recent_completions = crate::team::events::read_events(&crate::team::team_events_path(
            &self.config.project_root,
        ))?
        .into_iter()
        .rev()
        .filter(|event| event.event == "task_completed")
        .take(5)
        .map(|event| match (event.role, event.task) {
            (Some(role), Some(task)) => format!("{role} completed task #{task}"),
            (Some(role), None) => format!("{role} reported a completion"),
            _ => "recent completion recorded".to_string(),
        })
        .collect::<Vec<_>>();
        let (roadmap_context, project_goals) = planning_prompt_context(&self.config.project_root);
        let tact_prompt = crate::team::tact::TactPrompt {
            board_summary: board_summary.clone(),
            recent_completions: recent_completions.clone(),
            roadmap_priorities: roadmap_context.clone(),
            idle_count: idle_engineer_count,
            dispatchable_count: dispatchable_task_count,
        };
        let prompt = crate::team::tact::compose_planning_prompt_with_recovery_context(
            crate::team::tact::PlanningPromptRecoveryContext {
                idle_engineer_count,
                board_summary: &board_summary,
                recent_completions: &recent_completions,
                roadmap_context: &roadmap_context,
                project_goals: &project_goals,
                project_name: &self.config.team_config.name,
                blocked_task_summaries: &blocked_task_summaries,
                review_task_summaries: &review_drain_task_summaries,
            },
        );
        let body = format!(
            "HIGH PRIORITY: planning cycle triggered because idle engineers outnumber dispatchable tasks.\n\n{prompt}"
        );

        let inbox_root = inbox::inboxes_root(&self.config.project_root);
        let sender = self.automation_sender_for(&architect);
        let message = inbox::InboxMessage::new_send(&sender, &architect, &body);
        inbox::deliver_to_inbox(&inbox_root, &message)?;
        self.record_message_routed(&sender, &architect);
        self.record_tact_cycle_triggered(&architect, idle_engineer_count as u32, &board_summary);

        self.scheduler.planning_cycle_last_fired = Some(Instant::now());
        self.scheduler.planning_cycle_active = true;
        // #687 followup: persist immediately so a daemon restart within the
        // heartbeat window (5 min) doesn't fire a duplicate planning cycle
        // at the architect. Without this, hot-reload or manual restart
        // seconds after a cycle fires sends the architect two prompts for
        // the same "pipeline is dry" state, burning orchestrator tokens.
        if let Err(error) = self.persist_runtime_state(false) {
            warn!(error = %error, "failed to persist daemon state after planning cycle fire");
        }
        info!(
            architect,
            idle_engineers = idle_engineer_count,
            dispatchable_tasks = dispatchable_task_count,
            "triggered planning cycle"
        );
        self.record_orchestrator_action(format!(
            "planning: triggered planning cycle for {} (idle={}, dispatchable={})\n{}",
            architect,
            idle_engineer_count,
            dispatchable_task_count,
            crate::team::tact::compose_prompt(&tact_prompt)
        ));
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::team::daemon) fn maybe_trigger_planning_cycle(&mut self) -> Result<()> {
        self.tact_check()
    }

    pub(in crate::team::daemon) fn handle_planning_response(
        &mut self,
        response: &str,
    ) -> Result<usize> {
        let architect = self
            .config
            .members
            .iter()
            .find(|member| member.role_type == RoleType::Architect)
            .map(|member| member.name.clone())
            .unwrap_or_else(|| "architect".to_string());
        let latency_secs = self
            .scheduler
            .planning_cycle_last_fired
            .map(|instant| instant.elapsed().as_secs())
            .unwrap_or(0);
        let specs = crate::team::tact::parser::parse_planning_response(response);
        let board_dir = self.board_dir();
        let result = crate::team::tact::create_board_tasks(&specs, &board_dir)
            .map(|created: Vec<u32>| created.len());

        self.scheduler.planning_cycle_active = false;

        match result {
            Ok(created) => {
                // #681: track empty vs productive cycles so `tact_check` can
                // back off the cadence when the architect keeps returning
                // zero new tasks (blocked board, no dispatchable work).
                if created == 0 {
                    self.scheduler.planning_cycle_consecutive_empty = self
                        .scheduler
                        .planning_cycle_consecutive_empty
                        .saturating_add(1);
                    // #688: when a cycle is empty, reset the cooldown anchor to
                    // "now" so the next cycle is gated on time-since-completion,
                    // not time-since-fire. Without this a slow (10-min) empty
                    // cycle exactly hits the 2× cooldown boundary the moment
                    // it completes and a fresh cycle fires within the same tick.
                    // Productive cycles (created > 0) leave last_fired alone so
                    // the architect can keep planning as fast as the pipeline needs.
                    self.scheduler.planning_cycle_last_fired = Some(Instant::now());
                } else {
                    self.scheduler.planning_cycle_consecutive_empty = 0;
                }
                // #710: persist immediately so a non-clean daemon termination
                // (SIGKILL from `batty stop` timeout, panic, OOM) doesn't
                // revert consecutive_empty/last_fired back to the last
                // heartbeat snapshot — which shrinks the backoff cooldown and
                // lets a spurious planning cycle fire within minutes of the
                // restart. Observed in batty-marketing at 12:42:49 UTC: after
                // a forced restart at 12:34:03 lost the 12:33:17 empty
                // response update, the restored state carried stale
                // consecutive_empty=1 instead of 2, shrinking cooldown from
                // 900s → 600s and firing a fresh planning cycle 572s later.
                if let Err(error) = self.persist_runtime_state(false) {
                    warn!(
                        error = %error,
                        "failed to persist daemon state after planning cycle response"
                    );
                }
                self.record_tact_tasks_created(
                    &architect,
                    created as u32,
                    latency_secs,
                    true,
                    None,
                );
                info!(
                    created,
                    consecutive_empty = self.scheduler.planning_cycle_consecutive_empty,
                    "applied planning cycle response"
                );
                self.record_orchestrator_action(format!(
                    "planning: applied planning response and created {} tasks",
                    created
                ));
                Ok(created)
            }
            Err(error) => {
                self.record_tact_tasks_created(
                    &architect,
                    0,
                    latency_secs,
                    false,
                    Some(&error.to_string()),
                );
                Err(error)
            }
        }
    }

    /// Count engineers that are tmux-idle AND have no active board items.
    pub(in crate::team::daemon) fn truly_idle_engineer_count(
        &self,
        all_tasks: &[crate::task::Task],
    ) -> usize {
        let engineers_with_active_items: std::collections::HashSet<String> = all_tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "todo" | "in-progress" | "review"))
            .filter_map(|task| task.claimed_by.as_ref())
            .map(|name| name.trim_start_matches('@').to_string())
            .collect();

        self.idle_engineer_names()
            .into_iter()
            .filter(|name| !engineers_with_active_items.contains(name))
            .count()
    }
}
