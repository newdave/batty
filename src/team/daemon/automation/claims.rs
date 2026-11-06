//! Claim TTL management, claimed-branch mismatch alerts, and automation nudge delivery.

use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

use super::super::*;
use super::*;

const CLAIM_PROGRESS_AUDIT_KEY_PREFIX: &str = "claim-progress::";

const CLAIMED_BRANCH_MISMATCH_ALERT_KEY_PREFIX: &str = "claimed-branch-mismatch::";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::team::daemon) struct ClaimedTaskBranchMismatch {
    pub(in crate::team::daemon) task_id: u32,
    pub(in crate::team::daemon) expected_branch: String,
    pub(in crate::team::daemon) current_branch: String,
}

fn claim_time_held_secs(task: &crate::task::Task, now: DateTime<Utc>) -> Option<u64> {
    task.claimed_at
        .as_deref()
        .and_then(parse_rfc3339_utc)
        .and_then(|claimed_at| {
            let held_secs = now.signed_duration_since(claimed_at).num_seconds();
            (held_secs >= 0).then_some(held_secs as u64)
        })
}

pub(super) fn reset_claimed_worktree_to_base(
    work_dir: &std::path::Path,
    base_branch: &str,
    trunk_branch: &str,
) -> Result<crate::worktree::WorktreeResetReason> {
    let branch = current_worktree_branch(work_dir).unwrap_or_else(|_| base_branch.to_string());
    let commit_message = format!("wip: auto-save before worktree reset [{branch}]");
    crate::worktree::reset_worktree_to_base_with_options_for_trunk(
        work_dir,
        base_branch,
        &commit_message,
        Duration::from_secs(5),
        crate::worktree::PreserveFailureMode::SkipReset,
        "state-reconciliation/claimed-lane-reset",
        trunk_branch,
    )
    .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Branch the engineer's worktree should be on for `task`. A recorded
/// `branch` that belongs to another member (left behind when the task was
/// reassigned or its stale claim reclaimed) is ignored: a member's worktree
/// must only ever check out its own `<member>/<task>` branch.
pub(in crate::team) fn authoritative_task_branch(
    engineer: &str,
    task: &crate::task::Task,
) -> String {
    task.branch
        .clone()
        .filter(|branch| is_managed_task_branch(engineer, branch))
        .unwrap_or_else(|| crate::worktree::ops::engineer_task_branch_name(engineer, task.id))
}

pub(super) fn is_managed_task_branch(engineer: &str, branch: &str) -> bool {
    crate::worktree::ops::is_engineer_task_branch(branch, engineer)
}

pub(super) fn format_branch_recovery_blocker(reason: &str) -> String {
    format!("automatic branch recovery blocked: {reason}")
}

impl TeamDaemon {
    pub(in crate::team::daemon) fn claimed_task_branch_mismatch(
        &self,
        engineer: &str,
    ) -> Result<Option<ClaimedTaskBranchMismatch>> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
        else {
            return Ok(None);
        };
        if !member.use_worktrees {
            return Ok(None);
        }

        let tasks_dir = self.board_dir().join("tasks");
        if !tasks_dir.exists() {
            return Ok(None);
        }

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.exists() {
            return Ok(None);
        }

        let current_branch = match current_worktree_branch(&worktree_dir) {
            Ok(branch) => branch,
            Err(_) => return Ok(None),
        };

        let board_tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
        let claimed: Vec<&crate::task::Task> = board_tasks
            .iter()
            .filter(|task| {
                task.claimed_by.as_deref() == Some(engineer)
                    && super::super::interventions::task_needs_owned_intervention(
                        task.status.as_str(),
                    )
            })
            .collect();
        let Some(task) = select_authoritative_owned_task(
            self.active_task_id(engineer),
            (current_branch != "HEAD").then_some(current_branch.as_str()),
            &claimed,
        ) else {
            return Ok(None);
        };

        let expected_branch = authoritative_task_branch(engineer, task);
        if current_branch == expected_branch {
            return Ok(None);
        }

        Ok(Some(ClaimedTaskBranchMismatch {
            task_id: task.id,
            expected_branch,
            current_branch,
        }))
    }

    pub(in crate::team::daemon) fn alert_claimed_task_branch_mismatch(
        &mut self,
        engineer: &str,
        mismatch: &ClaimedTaskBranchMismatch,
        actionable_reason: Option<&str>,
    ) -> Result<()> {
        let cooldown_key = format!("{CLAIMED_BRANCH_MISMATCH_ALERT_KEY_PREFIX}{engineer}");
        let cooldown = Duration::from_secs(
            self.config
                .team_config
                .automation
                .intervention_cooldown_secs
                .max(60),
        );
        if self.scheduler.cooldown_active(&cooldown_key, cooldown) {
            return Ok(());
        }
        self.scheduler
            .intervention_cooldowns
            .insert(cooldown_key, Instant::now());

        let reason_suffix = actionable_reason
            .map(|reason| format!(" Reason: {reason}."))
            .unwrap_or_default();
        let alert = format!(
            "Reconciliation alert: {engineer} is claimed on task #{} but the worktree is on '{}' instead of '{}'. Batty will refuse new assignment and suppress automation nudges until the branch is corrected manually.{reason_suffix}",
            mismatch.task_id, mismatch.current_branch, mismatch.expected_branch
        );
        let manager = self.assignment_sender(engineer);
        let _ = self.queue_daemon_message(&manager, &alert);
        let _ = self.queue_daemon_message(
            engineer,
            &format!(
                "Branch/task mismatch detected for task #{}. Current branch: '{}'. Expected branch: '{}'. {} Resolve it manually before continuing.",
                mismatch.task_id,
                mismatch.current_branch,
                mismatch.expected_branch,
                actionable_reason.unwrap_or("Automatic recovery is paused.")
            ),
        );

        self.emit_event(TeamEvent::state_reconciliation(
            Some(engineer),
            Some(&mismatch.task_id.to_string()),
            "branch_mismatch",
        ));
        self.record_orchestrator_action(format!(
            "state reconciliation: detected branch/task mismatch for {} (task #{}, current '{}', expected '{}')",
            engineer, mismatch.task_id, mismatch.current_branch, mismatch.expected_branch
        ));
        Ok(())
    }

    pub(in crate::team) fn deliver_automation_nudge(
        &mut self,
        recipient: &str,
        body: &str,
    ) -> Result<MessageDelivery> {
        if self
            .config
            .members
            .iter()
            .any(|member| member.name == recipient && member.role_type == RoleType::Engineer)
            && let Some(mismatch) = self.claimed_task_branch_mismatch(recipient)?
        {
            self.alert_claimed_task_branch_mismatch(recipient, &mismatch, None)?;
            self.record_orchestrator_action(format!(
                "suppressed automation nudge for {} while branch/task mismatch is unresolved",
                recipient
            ));
            return Ok(MessageDelivery::OrchestratorLogged);
        }

        if self.config.team_config.orchestrator_enabled() {
            let summary = body
                .lines()
                .find_map(normalized_context_line)
                .unwrap_or_else(|| "automation nudge".to_string());
            self.record_orchestrator_action(format!("diverted nudge for {recipient}: {summary}"));
            return Ok(MessageDelivery::OrchestratorLogged);
        }

        self.queue_daemon_message(recipient, body)
    }

    pub(in crate::team::daemon) fn maybe_manage_task_claim_ttls(&mut self) -> Result<()> {
        let tasks_dir = self.board_dir().join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }

        let board_tasks = self.board_tasks()?;
        let now = Utc::now();
        let progress_interval = Duration::from_secs(
            self.config
                .team_config
                .workflow_policy
                .claim_ttl
                .progress_check_interval_secs,
        );

        for task in board_tasks
            .iter()
            .filter(|task| task.status == "in-progress")
            .filter(|task| task.claimed_by.is_some())
        {
            let Some(engineer) = task.claimed_by.as_deref() else {
                continue;
            };
            // #674 defect 3: skip TTL reclaim when the claimed engineer is
            // backend-parked (quota_exhausted with future retry_at). Parked
            // engineers cannot produce progress by definition — reclaiming
            // their tasks only rotates them to another quota-blocked
            // engineer and inflates board churn.
            if self
                .health
                .member_backend_parked(&self.config.project_root, engineer)
            {
                continue;
            }
            let Some(member) = self
                .config
                .members
                .iter()
                .find(|member| member.name == engineer)
                .cloned()
            else {
                continue;
            };
            let use_worktrees = member.use_worktrees;
            let work_dir = if use_worktrees {
                self.config
                    .project_root
                    .join(".batty")
                    .join("worktrees")
                    .join(engineer)
            } else {
                self.member_work_dir(&member)
            };
            let ttl_secs = self.claim_ttl_secs_for_priority(&task.priority);
            let current_output_bytes = self
                .shim_handles
                .get(engineer)
                .map(|handle| handle.output_bytes)
                .unwrap_or(0);

            if task.claim_expires_at.is_none()
                || task.last_progress_at.is_none()
                || task.claim_ttl_secs.is_none()
            {
                crate::team::task_cmd::initialize_task_claim(
                    &self.board_dir(),
                    task.id,
                    ttl_secs,
                    now,
                    current_output_bytes,
                )?;
                self.emit_event(TeamEvent::task_claim_created(
                    engineer,
                    &task.id.to_string(),
                    ttl_secs,
                    &(now + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339(),
                ));
                continue;
            }

            let progress_key = format!("{CLAIM_PROGRESS_AUDIT_KEY_PREFIX}{}", task.id);
            let should_check_progress = !self
                .scheduler
                .cooldown_active(&progress_key, progress_interval);
            let Some(expires_at) = task.claim_expires_at.as_deref().and_then(parse_rfc3339_utc)
            else {
                continue;
            };

            if should_check_progress
                && let Some(progress_type) = task_claim_progress_type(
                    task,
                    &work_dir,
                    current_output_bytes,
                    self.config.team_config.trunk_branch(),
                )
            {
                if expires_at <= now && progress_type == "dirty_files" {
                    self.scheduler
                        .intervention_cooldowns
                        .insert(progress_key.clone(), Instant::now());
                } else {
                    let extensions = task.claim_extensions.unwrap_or(0).saturating_add(1).min(
                        self.config
                            .team_config
                            .workflow_policy
                            .claim_ttl
                            .max_extensions,
                    );
                    crate::team::task_cmd::refresh_task_claim_progress(
                        &self.board_dir(),
                        task.id,
                        ttl_secs,
                        now,
                        current_output_bytes,
                        extensions,
                    )?;
                    self.emit_event(TeamEvent::task_claim_progress(
                        engineer,
                        &task.id.to_string(),
                        progress_type,
                    ));
                    self.emit_event(TeamEvent::task_claim_extended(
                        engineer,
                        &task.id.to_string(),
                        &(now + chrono::Duration::seconds(ttl_secs as i64)).to_rfc3339(),
                    ));
                    self.scheduler
                        .intervention_cooldowns
                        .insert(progress_key.clone(), Instant::now());
                    continue;
                }
            }

            self.scheduler
                .intervention_cooldowns
                .insert(progress_key, Instant::now());

            let expires_in_secs = expires_at.signed_duration_since(now).num_seconds().max(0) as u64;
            if expires_in_secs
                <= self
                    .config
                    .team_config
                    .workflow_policy
                    .claim_ttl
                    .warning_secs
                && task.claim_warning_sent_at.is_none()
            {
                crate::team::task_cmd::mark_task_claim_warning(&self.board_dir(), task.id, now)?;
                self.emit_event(TeamEvent::task_claim_warning(
                    engineer,
                    &task.id.to_string(),
                    expires_in_secs,
                ));
                let _ = self.queue_message(
                    "daemon",
                    engineer,
                    &format!(
                        "Your claim on task #{} expires in {} minutes. Commit your work or report a blocker to extend it.",
                        task.id,
                        (expires_in_secs / 60).max(1)
                    ),
                );
            }

            if expires_at > now {
                continue;
            }

            let time_held_secs = claim_time_held_secs(task, now);
            if use_worktrees {
                let base_branch = engineer_base_branch_name(engineer);
                let trunk_branch = self.config.team_config.trunk_branch().to_string();
                match reset_claimed_worktree_to_base(&work_dir, &base_branch, &trunk_branch) {
                    Ok(reason) if reason.reset_performed() => {
                        self.record_orchestrator_action(format!(
                            "claim ttl: reset {} worktree to {} before reclaiming task #{} ({})",
                            engineer,
                            base_branch,
                            task.id,
                            reason.as_str()
                        ))
                    }
                    Ok(reason) => {
                        warn!(
                            engineer = %engineer,
                            task_id = task.id,
                            worktree = %work_dir.display(),
                            reset_reason = reason.as_str(),
                            "claim ttl: skipped engineer worktree reset before reclaim"
                        );
                        self.report_preserve_failure(
                            engineer,
                            Some(task.id),
                            "TTL reclaim/reset",
                            reason.as_str(),
                        );
                        self.record_orchestrator_action(format!(
                            "claim ttl: skipped reclaim reset for {} task #{} ({})",
                            engineer,
                            task.id,
                            reason.as_str()
                        ));
                        continue;
                    }
                    Err(error) => {
                        warn!(
                            engineer = %engineer,
                            task_id = task.id,
                            worktree = %work_dir.display(),
                            error = %error,
                            "claim ttl: failed to reset engineer worktree before reclaim"
                        );
                        self.report_preserve_failure(
                            engineer,
                            Some(task.id),
                            "TTL reclaim/reset",
                            &error.to_string(),
                        );
                        continue;
                    }
                }
            }
            let branch = task
                .branch
                .clone()
                .or_else(|| current_worktree_branch(&work_dir).ok());
            let next_action = match branch.as_deref() {
                Some(branch) => {
                    format!(
                        "Reclaimed after TTL expiry. Previous work preserved on branch {branch}."
                    )
                }
                None => {
                    "Reclaimed after TTL expiry. Previous work preserved in the engineer worktree."
                        .to_string()
                }
            };
            crate::team::task_cmd::reclaim_task_claim_with_attribution(
                &self.board_dir(),
                task.id,
                &next_action,
                crate::team::task_cmd::StatusTransitionAttribution::daemon(
                    "daemon.automation.claim_ttl",
                ),
            )?;
            self.clear_active_task(engineer);
            self.emit_event(TeamEvent::task_claim_expired(
                engineer,
                &task.id.to_string(),
                true,
                time_held_secs,
            ));
            self.dispatch
                .recent_dispatches
                .insert((task.id, engineer.to_string()), Instant::now());
            let manager = self.assignment_sender(engineer);
            let _ = self.queue_message(
                "daemon",
                &manager,
                &format!(
                    "Task #{} reclaimed from {} after {} minutes with no progress.",
                    task.id,
                    engineer,
                    ttl_secs / 60
                ),
            );
            self.record_orchestrator_action(format!(
                "claim ttl: reclaimed task #{} from {} after {} minutes without progress",
                task.id,
                engineer,
                ttl_secs / 60
            ));
        }

        Ok(())
    }

    pub(super) fn maybe_reset_engineer_to_safe_branch(
        &mut self,
        engineer: &str,
        current_branch: &str,
        authorized_tasks: &[&crate::task::Task],
        completed_task: Option<&crate::task::Task>,
        reason: &str,
    ) {
        let base_branch = engineer_base_branch_name(engineer);
        if current_branch == "HEAD" || current_branch == base_branch {
            return;
        }
        if authorized_tasks
            .iter()
            .any(|task| task.branch.as_deref() == Some(current_branch))
        {
            return;
        }

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.exists() {
            return;
        }

        if let Some(task) = completed_task {
            let team_config_dir = self.config.project_root.join(".batty").join("team_config");
            match crate::team::task_loop::quarantine_completed_lane_for_recovery(
                &self.config.project_root,
                &worktree_dir,
                engineer,
                task,
                current_branch,
                reason,
                &team_config_dir,
                Duration::from_secs(10),
            ) {
                Ok(Some(record)) => {
                    self.emit_event(TeamEvent::worktree_reconciled(engineer, current_branch));
                    self.record_state_reconciliation(
                        Some(engineer),
                        Some(task.id),
                        "done_lane_reset",
                    );
                    self.record_orchestrator_action(format!(
                        "state reconciliation: quarantined completed task #{} for {} before reset ({})",
                        task.id,
                        engineer,
                        record.doctor_check_line()
                    ));
                }
                Ok(None) => {
                    self.emit_event(TeamEvent::worktree_reconciled(engineer, current_branch));
                    self.record_state_reconciliation(
                        Some(engineer),
                        Some(task.id),
                        "done_lane_reset",
                    );
                    self.record_orchestrator_action(format!(
                        "state reconciliation: reset completed task lane #{} for {} to '{}'",
                        task.id, engineer, base_branch
                    ));
                }
                Err(error) => {
                    self.report_preserve_failure(
                        engineer,
                        Some(task.id),
                        "completed-task cleanup/reset",
                        &error.to_string(),
                    );
                    self.record_orchestrator_action(format!(
                        "state reconciliation: blocked completed task lane #{} for {} ({error})",
                        task.id, engineer
                    ));
                }
            }
            return;
        }

        let trunk_branch = self.config.team_config.trunk_branch().to_string();
        match reset_claimed_worktree_to_base(&worktree_dir, &base_branch, &trunk_branch) {
            Ok(reset_reason) if reset_reason.reset_performed() => {
                info!(
                    engineer = %engineer,
                    stale_branch = %current_branch,
                    reset_to = %base_branch,
                    reset_reason = reset_reason.as_str(),
                    reason,
                    "state reconciliation reset engineer worktree to safe branch"
                );
                self.emit_event(TeamEvent::worktree_reconciled(engineer, current_branch));
                self.record_state_reconciliation(Some(engineer), None, "branch_reset");
                self.record_orchestrator_action(format!(
                    "state reconciliation: reset {engineer} from stale branch '{current_branch}' to '{base_branch}' ({reason}, {})",
                    reset_reason.as_str()
                ));
            }
            Ok(reset_reason) => {
                self.report_preserve_failure(
                    engineer,
                    None,
                    "safe-branch recovery",
                    reset_reason.as_str(),
                );
                self.record_orchestrator_action(format!(
                    "state reconciliation: skipped resetting {engineer} from stale branch '{current_branch}' to '{base_branch}' ({reason}, {})",
                    reset_reason.as_str()
                ))
            }
            Err(error) => {
                warn!(
                    engineer = %engineer,
                    branch = %current_branch,
                    reset_to = %base_branch,
                    error = %error,
                    reason,
                    "state reconciliation failed to reset engineer worktree to safe branch"
                );
                self.report_preserve_failure(
                    engineer,
                    None,
                    "safe-branch recovery",
                    &error.to_string(),
                );
            }
        }
    }

    pub(in crate::team::daemon) fn claim_ttl_secs_for_priority(&self, priority: &str) -> u64 {
        let policy = &self.config.team_config.workflow_policy.claim_ttl;
        if priority.eq_ignore_ascii_case("critical") {
            policy.critical_secs
        } else {
            policy.default_secs
        }
    }
}
