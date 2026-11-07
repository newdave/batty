//! Task aging alerts, dependency auto-unblock, and stale preserve-blocker cleanup.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use tracing::{info, warn};

use super::super::*;
use super::*;

pub(super) fn task_blocker_reason(
    task: &crate::task::Task,
    tasks: &[crate::task::Task],
) -> Option<String> {
    task.blocked_on
        .as_deref()
        .or(task.blocked.as_deref())
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
        .or_else(|| crate::team::resolver::dispatch_blocking_reason(task, tasks))
}

fn aging_cooldown_key(kind: &str, task_id: u32) -> String {
    format!("aging::{kind}::{task_id}")
}

pub(super) fn first_manager_name(members: &[MemberInstance]) -> Option<String> {
    members
        .iter()
        .find(|member| member.role_type == RoleType::Manager)
        .map(|member| member.name.clone())
}

/// A done or archived task still carrying the blocker `report_preserve_failure` sets.
pub(in crate::team::daemon) fn stale_preserve_blocker(task: &crate::task::Task) -> bool {
    if !matches!(task.status.as_str(), "done" | "archived") {
        return false;
    }
    [task.blocked_on.as_deref(), task.blocked.as_deref()]
        .into_iter()
        .flatten()
        .any(|reason| {
            reason.contains("preserve_failed_reset_skipped")
                || reason.contains("could not safely auto-save")
        })
}

impl TeamDaemon {
    pub(in crate::team::daemon) fn maybe_emit_task_aging_alerts(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }
        // #679: messaging below assumes a single git repo at `project_root`
        // (branch, commits ahead of main, etc.). When the project isn't a
        // git repo (or is a multi-repo parent), reframe the aging alert to
        // avoid referencing branches that don't exist.
        let project_is_git = self.is_git_repo && !self.is_multi_repo;

        let thresholds = crate::team::board::AgingThresholds {
            stale_in_progress_hours: self
                .config
                .team_config
                .workflow_policy
                .stale_in_progress_hours,
            aged_todo_hours: self.config.team_config.workflow_policy.aged_todo_hours,
            stale_review_hours: self.config.team_config.workflow_policy.stale_review_hours,
        };
        let tasks = self.board_tasks()?;
        let report = crate::team::board::compute_task_aging_from_tasks(
            &tasks,
            &self.config.project_root,
            thresholds,
            Utc::now(),
        )?;
        let tasks_by_id = tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<HashMap<_, _>>();
        let (active_stale_review, suppressed_stale_review) =
            active_stale_review_entries(&report, &tasks_by_id);
        for task_id in suppressed_stale_review {
            self.record_state_reconciliation(None, Some(task_id), "review_fix");
            self.record_orchestrator_action(format!(
                "state reconciliation: suppressed stale review aging alert for task #{}",
                task_id
            ));
        }
        let progress_window = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        let now = Utc::now();

        let active_keys: HashSet<String> = report
            .stale_in_progress
            .iter()
            .map(|task| aging_cooldown_key("task_stale", task.task_id))
            .chain(
                report
                    .stale_in_progress
                    .iter()
                    .map(|task| aging_cooldown_key("task_checkpoint", task.task_id)),
            )
            .chain(
                report
                    .aged_todo
                    .iter()
                    .map(|task| aging_cooldown_key("task_aged", task.task_id)),
            )
            .chain(
                active_stale_review
                    .iter()
                    .map(|task| aging_cooldown_key("review_stale", task.task_id)),
            )
            .collect();
        self.scheduler
            .intervention_cooldowns
            .retain(|key, _| !key.starts_with("aging::") || active_keys.contains(key));

        for task in &report.stale_in_progress {
            let stale_key = aging_cooldown_key("task_stale", task.task_id);
            let checkpoint_key = aging_cooldown_key("task_checkpoint", task.task_id);

            let owner = task.claimed_by.as_deref().unwrap_or("unassigned");
            // #674 defect 4: do not escalate stuck-task or emit task_stale
            // alerts when the owner is backend-parked. Parked engineers are
            // waiting, not stalled — escalating produces board churn and
            // noisy telemetry during already-degraded windows. Also clear
            // the cooldown markers so that when the engineer recovers we
            // start aging bookkeeping from a clean slate.
            if task.claimed_by.as_deref().is_some_and(|name| {
                self.health
                    .member_backend_parked(&self.config.project_root, name)
            }) {
                self.scheduler.intervention_cooldowns.remove(&stale_key);
                self.scheduler
                    .intervention_cooldowns
                    .remove(&checkpoint_key);
                continue;
            }
            let liveness = tasks_by_id.get(&task.task_id).and_then(|board_task| {
                self.in_progress_task_liveness(board_task, now, progress_window)
            });

            if let Some(liveness) = liveness {
                if liveness.has_live_progress() {
                    self.scheduler.intervention_cooldowns.remove(&stale_key);
                    self.scheduler
                        .intervention_cooldowns
                        .remove(&checkpoint_key);
                    continue;
                }

                if liveness.dirty_worktree {
                    let checkpoint_sent = self
                        .scheduler
                        .intervention_cooldowns
                        .contains_key(&checkpoint_key);
                    if !checkpoint_sent {
                        let body = if project_is_git {
                            format!(
                                "Task #{} is still in progress and has dirty worktree changes, but the branch has no commits ahead of `main`.\nTask: {}\nNext step: create a checkpoint commit now so Batty does not escalate this lane as stale.\n```\ngit add -A\ngit commit -m \"wip: checkpoint task #{}\"\n```",
                                task.task_id, task.title, task.task_id
                            )
                        } else {
                            format!(
                                "Task #{} is still in progress with uncommitted changes, but no progress signals have been observed during the cooldown window.\nTask: {}\nNext step: save a checkpoint of your work so Batty does not escalate this lane as stale.",
                                task.task_id, task.title
                            )
                        };
                        let _ = self.queue_daemon_message(owner, &body);
                        self.record_orchestrator_action(format!(
                            "aging: requested checkpoint commit for task #{} ({owner})",
                            task.task_id
                        ));
                        self.scheduler
                            .intervention_cooldowns
                            .insert(checkpoint_key, Instant::now());
                        self.scheduler.intervention_cooldowns.remove(&stale_key);
                        continue;
                    }
                    if self.aging_alert_on_cooldown(&checkpoint_key) {
                        continue;
                    }
                } else {
                    self.scheduler
                        .intervention_cooldowns
                        .remove(&checkpoint_key);
                }
            }

            if self.aging_alert_on_cooldown(&stale_key) {
                continue;
            }

            let (reason_code, reason, manager_reason) = if liveness
                .is_some_and(|state| state.dirty_worktree)
            {
                (
                    "commit_stale_with_dirty_work",
                    format!(
                        "commit stale after {}s with dirty worktree changes and no claim progress during the cooldown window",
                        task.age_secs
                    ),
                    "commit_stale_with_dirty_work",
                )
            } else if project_is_git {
                (
                    "progress_stale",
                    format!(
                        "progress stale after {}s with no commits ahead of main and no claim progress during the cooldown window",
                        task.age_secs
                    ),
                    "progress_stale",
                )
            } else {
                (
                    "progress_stale",
                    format!(
                        "progress stale after {}s with no progress signals during the cooldown window",
                        task.age_secs
                    ),
                    "progress_stale",
                )
            };
            self.emit_event(TeamEvent::task_stale(
                owner,
                &task.task_id.to_string(),
                &reason,
            ));
            self.record_task_escalated(owner, task.task_id.to_string(), Some(reason_code));

            if let Some(recipient) = task
                .claimed_by
                .as_deref()
                .and_then(|owner| self.manager_for_member_name(owner))
                .map(str::to_string)
                .or_else(|| first_manager_name(&self.config.members))
            {
                let body = if project_is_git {
                    format!(
                        "Task #{} has been in progress for {}s with no commits ahead of `main`.\nTask: {}\nOwner: {}\nReason: {}\nNext step: intervene, split the task, or confirm the engineer is still making progress.",
                        task.task_id, task.age_secs, task.title, owner, manager_reason
                    )
                } else {
                    format!(
                        "Task #{} has been in progress for {}s with no progress signals.\nTask: {}\nOwner: {}\nReason: {}\nNext step: intervene, split the task, or confirm the engineer is still making progress.",
                        task.task_id, task.age_secs, task.title, owner, manager_reason
                    )
                };
                let _ = self.queue_daemon_message(&recipient, &body);
            }

            self.scheduler
                .intervention_cooldowns
                .insert(stale_key, Instant::now());
        }

        for task in &report.aged_todo {
            let key = aging_cooldown_key("task_aged", task.task_id);
            if self.aging_alert_on_cooldown(&key) {
                continue;
            }

            let reason = format!("todo task aged {}s without movement", task.age_secs);
            self.emit_event(TeamEvent::task_aged(&task.task_id.to_string(), &reason));
            self.scheduler
                .intervention_cooldowns
                .insert(key, Instant::now());
        }

        for task in &active_stale_review {
            let key = aging_cooldown_key("review_stale", task.task_id);
            if self.aging_alert_on_cooldown(&key) {
                continue;
            }

            let Some(current_task) = current_review_task(&tasks_by_id, task.task_id) else {
                continue;
            };
            let review_owner = Some(current_task)
                .and_then(|task| task.review_owner.as_deref())
                .map(str::to_string)
                .or_else(|| {
                    current_task
                        .claimed_by
                        .as_deref()
                        .and_then(|owner| self.manager_for_member_name(owner))
                        .map(str::to_string)
                })
                .or_else(|| first_manager_name(&self.config.members));
            let review_state = crate::team::review::classify_review_task(
                &self.config.project_root,
                current_task,
                &tasks,
            );
            let reason = match &review_state {
                crate::team::review::ReviewQueueState::Current => {
                    format!("review queue stale after {}s", task.age_secs)
                }
                crate::team::review::ReviewQueueState::Stale(stale) => format!(
                    "stale review normalization after {}s: {}",
                    task.age_secs, stale.reason
                ),
            };
            self.emit_event(TeamEvent::review_stale(&task.task_id.to_string(), &reason));

            if let Some(recipient) = review_owner {
                let body = match review_state {
                    crate::team::review::ReviewQueueState::Current => format!(
                        "Review urgency: task #{} has been in review for {}s.\nTask: {}\nNext step: merge it, request rework, or escalate immediately.",
                        task.task_id, task.age_secs, current_task.title
                    ),
                    crate::team::review::ReviewQueueState::Stale(stale) => format!(
                        "Stale review normalization: task #{} has been stuck in review for {}s.\nTask: {}\nReason: {}\nNext step: {}.",
                        task.task_id,
                        task.age_secs,
                        current_task.title,
                        stale.reason,
                        stale.status_next_action()
                    ),
                };
                let _ = self.queue_daemon_message(&recipient, &body);
            }

            self.scheduler
                .intervention_cooldowns
                .insert(key, Instant::now());
        }

        Ok(())
    }

    pub(in crate::team::daemon) fn maybe_auto_unblock_blocked_tasks(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let tasks = self.board_tasks()?;
        let done_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|task| matches!(task.status.as_str(), "done" | "archived"))
            .map(|task| task.id)
            .collect();
        let unblocked_tasks = tasks
            .iter()
            .filter(|task| !task.depends_on.is_empty())
            .filter(|task| {
                task.depends_on
                    .iter()
                    .all(|dependency| done_task_ids.contains(dependency))
            })
            // A task blocked at the bench-recovery reset cap waits for a
            // human; finished dependencies must not send it back to todo.
            .filter(|task| {
                !crate::team::bench_recovery::read_bench_recovery(&task.source_path)
                    .bench_reset_capped
            })
            .filter(|task| match task.status.as_str() {
                "blocked" => true,
                "todo" | "backlog" | "runnable" => {
                    task.blocked.is_some() || task.blocked_on.is_some() || task.claimed_by.is_some()
                }
                _ => false,
            })
            .map(|task| {
                (
                    task.id,
                    task.title.clone(),
                    task.status.clone(),
                    task.depends_on.clone(),
                    self.auto_unblock_notification_recipient(task),
                )
            })
            .collect::<Vec<_>>();

        for (task_id, title, original_status, dependencies, recipient) in unblocked_tasks {
            let current_task = crate::task::load_task_by_id(&board_dir.join("tasks"), task_id)
                .with_context(|| format!("failed to reload task #{task_id} before auto-unblock"))?;
            if current_task.status == "blocked" {
                task_cmd::transition_task_with_attribution(
                    &board_dir,
                    task_id,
                    "todo",
                    task_cmd::StatusTransitionAttribution::daemon("daemon.automation.auto_unblock"),
                )
                .with_context(|| format!("failed to auto-unblock task #{task_id}"))?;
            } else {
                task_cmd::clear_blocked_fields(&board_dir, task_id).with_context(|| {
                    format!("failed to clear stale blocked fields for task #{task_id}")
                })?;
            }
            let current_task = crate::task::load_task_by_id(&board_dir.join("tasks"), task_id)
                .with_context(|| format!("failed to reload task #{task_id} after auto-unblock"))?;
            if current_task.status != "in-progress" {
                task_cmd::unclaim_task(&board_dir, task_id)
                    .with_context(|| format!("failed to clear stale claim for task #{task_id}"))?;
            }

            let dependency_list = dependencies
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let event_role = recipient.as_deref().unwrap_or("daemon");
            self.record_task_unblocked(event_role, task_id.to_string());
            self.record_orchestrator_action(format!(
                "dependency resolution: auto-unblocked task #{} ({}) after dependencies [{}] completed",
                task_id, title, dependency_list
            ));
            info!(
                task_id,
                task_title = %title,
                dependencies = %dependency_list,
                recipient = recipient.as_deref().unwrap_or("none"),
                "auto-unblocked blocked task"
            );

            let Some(recipient) = recipient else {
                continue;
            };
            let body = if original_status == "blocked" {
                format!(
                    "Task #{task_id} ({title}) was automatically moved from `blocked` to `todo` because dependencies [{dependency_list}] are done."
                )
            } else {
                format!(
                    "Task #{task_id} ({title}) was automatically released for dispatch because dependencies [{dependency_list}] are done."
                )
            };
            if let Err(error) = self.queue_daemon_message(&recipient, &body) {
                warn!(
                    task_id,
                    to = %recipient,
                    error = %error,
                    "failed to notify auto-unblocked task recipient"
                );
            }
        }

        Ok(())
    }

    /// Clear the blocker a failed worktree preservation left on a task that has since finished.
    /// The lane was dirty during a recovery, so Batty blocked the task rather than reset it; the
    /// work was then committed and merged, but nothing cleared the blocker. Clear it once the
    /// task is done or archived and its work is on the trunk: a recorded commit or branch must be
    /// merged into the trunk; with neither recorded, done is enough, because a task is marked done
    /// only after its merge. A recorded commit not on the trunk keeps the blocker, since that work
    /// might be lost. Only the blocking fields change: status and claim stay.
    pub(in crate::team::daemon) fn maybe_clear_stale_preserve_blockers(&mut self) -> Result<()> {
        let board_dir = self
            .config
            .project_root
            .join(".batty")
            .join("team_config")
            .join("board");
        let trunk = self.config.team_config.trunk_branch().to_string();
        let tasks = self.board_tasks()?;
        for task in tasks.iter() {
            if !stale_preserve_blocker(task) {
                continue;
            }
            let recorded = task
                .commit
                .as_deref()
                .or(task.branch.as_deref())
                .filter(|reference| !reference.is_empty());
            if let Some(reference) = recorded {
                let merged = crate::team::task_loop::branch_is_merged_into(
                    &self.config.project_root,
                    reference,
                    &trunk,
                )
                .unwrap_or(false);
                if !merged {
                    continue;
                }
            }
            task_cmd::clear_blocked_fields(&board_dir, task.id).with_context(|| {
                format!(
                    "failed to clear stale preservation blocker for task #{}",
                    task.id
                )
            })?;
            self.record_orchestrator_action(format!(
                "cleared stale worktree preservation blocker on done task #{} ({})",
                task.id, task.title
            ));
            info!(task_id = task.id, task_title = %task.title, "cleared stale preservation blocker");
        }
        Ok(())
    }

    pub(in crate::team::daemon) fn manager_for_member_name(
        &self,
        member_name: &str,
    ) -> Option<&str> {
        self.config
            .members
            .iter()
            .find(|member| member.name == member_name)
            .and_then(|member| member.reports_to.as_deref())
    }

    pub(in crate::team::daemon) fn auto_unblock_notification_recipient(
        &self,
        task: &crate::task::Task,
    ) -> Option<String> {
        task.claimed_by
            .as_deref()
            .filter(|owner| {
                self.config
                    .members
                    .iter()
                    .any(|member| member.name == *owner)
            })
            .map(str::to_string)
            .or_else(|| {
                self.config
                    .members
                    .iter()
                    .find(|member| member.role_type == RoleType::Manager)
                    .map(|member| member.name.clone())
            })
    }

    fn aging_alert_on_cooldown(&self, key: &str) -> bool {
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs,
        );
        self.scheduler.cooldown_active(key, cooldown)
    }
}
