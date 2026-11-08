//! State reconciliation between the board, engineer worktrees, and daemon state.

use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{info, warn};

use super::super::*;
use super::*;

const STATE_RECONCILIATION_AUDIT_KEY: &str = "state-reconciliation::audit";

const ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX: &str = "orphan-branch-mismatch::";

const DEFAULT_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS: u32 = 3;

fn orphan_branch_mismatch_max_attempts() -> u32 {
    std::env::var("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS)
}

pub(super) fn verification_retry_required_task(task: &crate::task::Task) -> bool {
    crate::team::board::read_workflow_metadata(&task.source_path).is_ok_and(|metadata| {
        metadata.tests_passed == Some(false)
            && metadata.outcome.as_deref() == Some("verification_retry_required")
            && !metadata.artifacts.is_empty()
    })
}

impl TeamDaemon {
    /// A transport turn ending is not authority to complete a blocked or reassigned task.
    /// Read the board afresh: reconciliation and completion events can race.
    pub(crate) fn completion_claim_is_current(&self, engineer: &str, task_id: u32) -> Result<bool> {
        // A newer turn invalidates the previous completion even if HEAD is unchanged.
        if self
            .shim_handles
            .get(engineer)
            .is_some_and(|handle| !handle.is_ready() || !handle.in_flight_messages.is_empty())
        {
            return Ok(false);
        }
        let tasks = crate::task::load_tasks_from_dir(&self.board_dir().join("tasks"))?;
        Ok(tasks.iter().any(|task| {
            task.id == task_id
                && task.status == "in-progress"
                && task.claimed_by.as_deref().is_some_and(|claim| {
                    super::super::health::resolve_engineer_claim(&self.config.members, claim)
                        == Some(engineer)
                })
        }))
    }

    pub(in crate::team::daemon) fn reconcile_active_tasks(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        let mut board_tasks = if tasks_dir.exists() {
            self.board_tasks()?
        } else {
            std::sync::Arc::new(Vec::new())
        };
        let audit_due = self.state_reconciliation_audit_due();
        if audit_due {
            self.mark_state_reconciliation_audit();
        }
        let stale: Vec<(String, u32, &'static str, bool)> = self
            .active_tasks
            .iter()
            .filter_map(|(engineer, task_id)| {
                let task_id = *task_id;
                match board_tasks.iter().find(|t| t.id == task_id) {
                    Some(task) if task.status == "done" => {
                        Some((engineer.clone(), task_id, "task is done", false))
                    }
                    Some(task) if task.status == "archived" => {
                        Some((engineer.clone(), task_id, "task is archived", false))
                    }
                    Some(task) if task.status == "review" => {
                        Some((engineer.clone(), task_id, "task entered review", true))
                    }
                    Some(task) if task.status == "blocked" => {
                        Some((engineer.clone(), task_id, "task entered blocked", true))
                    }
                    None => Some((engineer.clone(), task_id, "task no longer exists", false)),
                    // #698: resolve role-name claims ("alex-dev") to the instance-name
                    // that active_tasks is keyed on ("alex-dev-1-1") before comparing.
                    // Without this, auto-doctor's post-hot-reload re-attachment writes
                    // the instance name, then reconciliation falsely flags it as stale
                    // because the board still records the role name, cycling every tick.
                    Some(task)
                        if {
                            let claim = task.claimed_by.as_deref();
                            let resolved = claim.and_then(|c| {
                                super::super::health::resolve_engineer_claim(
                                    &self.config.members,
                                    c,
                                )
                            });
                            claim != Some(engineer.as_str()) && resolved != Some(engineer.as_str())
                        } =>
                    {
                        Some((
                            engineer.clone(),
                            task_id,
                            "task no longer claimed by this engineer",
                            false,
                        ))
                    }
                    // Clear if the task has been in todo/backlog for more than 60 seconds.
                    // This gives dispatch time to transition it to in-progress before
                    // reconciliation clears it. Without the delay, dispatch assigns a task,
                    // reconciliation immediately clears the active entry, and dispatch
                    // re-assigns in an infinite loop.
                    _ => None,
                }
            })
            .collect();
        let mut released_claims = false;
        for (engineer, task_id, reason, release_claim) in stale {
            if matches!(reason, "task is done" | "task is archived") {
                if let Some(task) = board_tasks.iter().find(|task| task.id == task_id) {
                    let worktree_dir = self.worktree_dir(&engineer);
                    if worktree_dir.exists() {
                        let source_branch = authoritative_task_branch(&engineer, task);
                        let team_config_dir =
                            self.config.project_root.join(".batty").join("team_config");
                        match crate::team::task_loop::quarantine_completed_lane_for_recovery(
                            &self.config.project_root,
                            &worktree_dir,
                            &engineer,
                            task,
                            &source_branch,
                            "completed task no longer needs engineer lane",
                            &team_config_dir,
                            Duration::from_secs(10),
                        ) {
                            Ok(Some(record)) => {
                                self.record_orchestrator_action(format!(
                                    "state reconciliation: preserved terminal task #{} for {} before cleanup ({})",
                                    task_id,
                                    engineer,
                                    record.doctor_check_line()
                                ));
                            }
                            Ok(None) => {}
                            Err(error) => {
                                self.report_preserve_failure(
                                    &engineer,
                                    Some(task_id),
                                    "completed-task cleanup/reset",
                                    &error.to_string(),
                                );
                                self.record_orchestrator_action(format!(
                                    "state reconciliation: blocked completed task #{} cleanup for {} ({error})",
                                    task_id, engineer
                                ));
                                continue;
                            }
                        }
                    }
                }
            }
            info!(
                engineer = %engineer,
                task_id,
                reason,
                "Reconciled stale active_task: {engineer} was tracking task #{task_id} ({reason})"
            );
            let correction = match reason {
                "task is done" => "clear_done",
                "task is archived" => "clear_archived",
                "task no longer exists" => "clear_missing",
                "task no longer claimed by this engineer" => "clear_unassigned",
                "task entered review" => "release_review",
                "task entered blocked" => "release_blocked",
                _ => "clear",
            };
            if release_claim {
                crate::team::task_cmd::release_engineer_claim(&board_dir, task_id)?;
                released_claims = true;
            }
            self.record_state_reconciliation(Some(&engineer), Some(task_id), correction);
            // #697: when the engineer's claim was cleared by the engineer
            // themselves (not by a task transition to done/review/blocked),
            // record an exclusion so the dispatcher does not immediately
            // re-dispatch the same task back to the engineer who parked it.
            if reason == "task no longer claimed by this engineer" {
                self.dispatch.record_task_release_by(
                    task_id,
                    &engineer,
                    self.release_exclusion_window(),
                );
            }
            if !self
                .config
                .members
                .iter()
                .any(|member| member.name == engineer)
                && let Err(error) = self.cleanup_removed_member_worktree(&engineer)
            {
                warn!(
                    engineer = %engineer,
                    task_id,
                    error = %error,
                    "state reconciliation failed to clean worktree for stale unconfigured owner"
                );
            }
            self.record_orchestrator_action(format!(
                "state reconciliation: {} stale active task #{} for {} ({})",
                if release_claim {
                    "released engineer from"
                } else {
                    "cleared"
                },
                task_id,
                engineer,
                reason
            ));
            self.clear_active_task(&engineer);
        }
        if released_claims {
            board_tasks = if tasks_dir.exists() {
                self.board_tasks()?
            } else {
                std::sync::Arc::new(Vec::new())
            };
        }

        let engineer_names: Vec<String> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer)
            .map(|m| m.name.clone())
            .collect();
        for eng in &engineer_names {
            let released_excess_claims = self.normalize_engineer_active_task_ownership(
                &board_tasks,
                &board_dir,
                eng,
                audit_due,
            )?;
            if released_excess_claims {
                board_tasks = if tasks_dir.exists() {
                    self.board_tasks()?
                } else {
                    std::sync::Arc::new(Vec::new())
                };
            }
        }

        // Skip tasks that are currently tracked in active_tasks — those were just
        // dispatched this cycle and the board file may not reflect the claim yet.
        let actively_tracked: std::collections::HashSet<u32> =
            self.active_tasks.values().copied().collect();

        // Orphaned review rescue: tasks in "review" that are stuck because
        // they have no review_owner (nobody assigned to review). Review tasks
        // no longer require claimed_by once the engineer is auto-released.
        for task in board_tasks.iter() {
            if task.status == "review"
                && task.review_owner.is_none()
                && !actively_tracked.contains(&task.id)
            {
                // #704: grace period before bouncing a fresh review back to
                // todo. Without this, the sequence was: engineer completes
                // draft → moves task to review → same reconciliation pass
                // clears the engineer's active_task (status=review) → the
                // loop below sees no review_owner (manager hasn't picked it
                // up yet) and bounces it. Observed batty-marketing
                // 2026-04-17 10:35:16 UTC on task #528: kai drafted in ~3m,
                // moved to review, orphan-rescue bounced it to todo before
                // jordan-pm had a chance to audit. Same pattern hit #528 at
                // 10:23:48, #526 at 08:49:12, #553 at 09:17:21, #525 at
                // 06:14:37 — 28 events total in one daemon log. Use the
                // task file's mtime as a proxy for "last state change":
                // kanban-md edits the file on every transition, so a fresh
                // review has mtime within seconds. Skip rescue until mtime
                // is older than REVIEW_RESCUE_GRACE_SECS.
                pub(super) const REVIEW_RESCUE_GRACE_SECS: u64 = 600;
                if task_file_age_secs(&task.source_path)
                    .is_some_and(|age| age < REVIEW_RESCUE_GRACE_SECS)
                {
                    continue;
                }
                // #698: preserve engineer-set block metadata across rescue.
                // Without this, the pair of `transition_task` calls below
                // clears `blocked` / `block_reason` / `blocked_on` (via
                // `clear_blocked`), re-dispatching a task the engineer or
                // manager explicitly parked — the canonical sam-designer
                // auto-refuse cascade we saw with batty-marketing #553.
                let preserve_block = task.blocked.is_some() || task.blocked_on.is_some();
                warn!(
                    task_id = task.id,
                    preserve_block,
                    "orphaned review task #{} has no owner — moving back to todo",
                    task.id
                );
                if preserve_block {
                    let _ =
                        crate::team::task_cmd::transition_task_preserving_block_with_attribution(
                            &board_dir,
                            task.id,
                            "in-progress",
                            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                                "daemon.automation.orphan_review_rescue",
                            ),
                        );
                    let _ =
                        crate::team::task_cmd::transition_task_preserving_block_with_attribution(
                            &board_dir,
                            task.id,
                            "todo",
                            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                                "daemon.automation.orphan_review_rescue",
                            ),
                        );
                } else {
                    let _ = crate::team::task_cmd::transition_task_with_attribution(
                        &board_dir,
                        task.id,
                        "in-progress",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.automation.orphan_review_rescue",
                        ),
                    );
                    let _ = crate::team::task_cmd::transition_task_with_attribution(
                        &board_dir,
                        task.id,
                        "todo",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.automation.orphan_review_rescue",
                        ),
                    );
                }
                let _ = crate::team::task_cmd::unclaim_task(&board_dir, task.id);
                // #684 / #686: same exponential-backoff dispatch-cooldown
                // pattern as in-progress rescue.
                self.dispatch
                    .record_task_rescue(task.id, self.orphan_rescue_cooldown());
            }
        }

        // Orphaned in-progress rescue: tasks in "in-progress" with no claimed_by.
        let rescue_base_cooldown =
            Duration::from_secs(self.config.team_config.board.orphan_rescue_cooldown_secs);
        for task in board_tasks.iter() {
            if task.status == "in-progress"
                && task.claimed_by.is_none()
                && !actively_tracked.contains(&task.id)
            {
                // #714: if this task was already rescued within the cascade
                // window, it's cycling through engineers who each claim and
                // release in seconds. Observed batty-marketing 2026-04-16
                // 23:09: #523 bounced alex-dev → kai-devrel → sam-designer →
                // priya-writer in 4 minutes, producing 10 rescues in one day;
                // #572 bounced sam-designer → alex-dev in 6 minutes on
                // 2026-04-17. The 300s task-level cooldown alone doesn't hold
                // because (a) per-engineer release exclusion (#697) lets the
                // task hop to a different engineer once the task-level gate
                // opens, and (b) races between `record_task_rescue` and
                // already-queued dispatch entries can slip one dispatch past
                // the gate before the cooldown is observed. Escalate to
                // `blocked` on the 2nd rescue so the manager triages the spec
                // (likely wrong assignee/role, ambiguous ownership, or a task
                // already mid-gate-pass that engineers keep refusing) before
                // any further dispatch. Tasks filed `blocked` are excluded
                // from `available_dispatch_tasks`, halting the cascade.
                let is_repeat_in_cascade = self
                    .dispatch
                    .recently_rescued_tasks
                    .get(&task.id)
                    .is_some_and(|record| record.in_cascade_window(rescue_base_cooldown));
                if is_repeat_in_cascade {
                    let block_reason = concat!(
                        "orphan-rescue cascade: task was claimed and released repeatedly by different engineers ",
                        "within the cascade window. Likely causes: wrong role/assignee, ambiguous ownership, ",
                        "or the task body already describes completed/parked work. Manager triage required ",
                        "before returning to todo."
                    )
                    .to_string();
                    warn!(
                        task_id = task.id,
                        "orphan-rescue cascade detected — moving task #{} to blocked for manager triage",
                        task.id
                    );
                    match crate::team::task_cmd::block_task_with_reason_and_attribution(
                        &board_dir,
                        task.id,
                        &block_reason,
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.automation.orphan_rescue_cascade",
                        ),
                    ) {
                        Ok(()) => {
                            if let Some(manager) = first_manager_name(&self.config.members) {
                                let body = format!(
                                    "Task #{} has been orphan-rescued repeatedly — engineers keep claiming it \
                                     and releasing within seconds. I moved it to `blocked` so dispatch stops \
                                     re-cycling it. Please review the task body and either (a) correct the \
                                     assignee/role if it's been routed to the wrong discipline, (b) split or \
                                     clarify the spec if engineers are refusing because ownership is ambiguous, \
                                     or (c) transition it to done if the body already documents gate-pass or \
                                     parked state. Then unblock it (remove `blocked`/`blocked_on`) or move \
                                     directly to the correct terminal status.",
                                    task.id
                                );
                                let inbox_root = inbox::inboxes_root(&self.config.project_root);
                                let sender = self.automation_sender_for(&manager);
                                let message =
                                    inbox::InboxMessage::new_send(&sender, &manager, &body);
                                match inbox::deliver_to_inbox(&inbox_root, &message) {
                                    Ok(_) => {
                                        self.record_message_routed(&sender, &manager);
                                    }
                                    Err(error) => {
                                        warn!(
                                            error = %error,
                                            manager,
                                            task_id = task.id,
                                            "failed to notify manager of orphan-rescue cascade escalation"
                                        );
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            warn!(
                                task_id = task.id,
                                error = %error,
                                "failed to escalate orphan-rescue cascade task to blocked"
                            );
                        }
                    }
                    // Still record the rescue so the cooldown grows if the
                    // manager unblocks it and the pattern repeats.
                    self.dispatch
                        .record_task_rescue(task.id, self.orphan_rescue_cooldown());
                    continue;
                }
                // #698: same block-preservation logic as review rescue above.
                let preserve_block = task.blocked.is_some() || task.blocked_on.is_some();
                warn!(
                    task_id = task.id,
                    preserve_block,
                    "orphaned in-progress task #{} has no owner — moving back to todo",
                    task.id
                );
                if preserve_block {
                    let _ =
                        crate::team::task_cmd::transition_task_preserving_block_with_attribution(
                            &board_dir,
                            task.id,
                            "todo",
                            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                                "daemon.automation.orphan_in_progress_rescue",
                            ),
                        );
                } else {
                    let _ = crate::team::task_cmd::transition_task_with_attribution(
                        &board_dir,
                        task.id,
                        "todo",
                        crate::team::task_cmd::StatusTransitionAttribution::daemon(
                            "daemon.automation.orphan_in_progress_rescue",
                        ),
                    );
                }
                // #684 / #686: hold the task off the dispatch queue with an
                // exponentially-growing cooldown on repeated rescues so the
                // releasing engineer or manager has a widening window to
                // reclaim/re-route before it auto-dispatches to a peer.
                self.dispatch
                    .record_task_rescue(task.id, self.orphan_rescue_cooldown());
            }
        }

        if let Err(error) = self.cleanup_unconfigured_member_worktrees() {
            warn!(
                error = %error,
                "state reconciliation failed to clean unconfigured member worktrees"
            );
        }

        Ok(())
    }

    fn state_reconciliation_audit_due(&self) -> bool {
        let interval = Duration::from_secs(
            self.config
                .team_config
                .board
                .state_reconciliation_interval_secs,
        );
        !self
            .scheduler
            .cooldown_active(STATE_RECONCILIATION_AUDIT_KEY, interval)
    }

    fn mark_state_reconciliation_audit(&mut self) {
        self.scheduler
            .intervention_cooldowns
            .insert(STATE_RECONCILIATION_AUDIT_KEY.to_string(), Instant::now());
    }

    pub(super) fn maybe_align_engineer_worktree_with_task(
        &mut self,
        engineer: &str,
        task: &crate::task::Task,
    ) -> Result<()> {
        let Some(member) = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
        else {
            return Ok(());
        };
        if !member.use_worktrees {
            return Ok(());
        }
        // Self-heal runs for both Idle and Working members (#666). Previously
        // we skipped when the member was Working, to avoid interrupting a live
        // session — but that left engineers permanently wedged when a branch
        // mismatch appeared on a non-idle lane: the "branch recovery blocked"
        // signal would re-emit every monitor tick with no healing action,
        // because the engineer's state was still Working despite the lane
        // being unable to make progress. Auto-saving dirty work as a wip
        // commit before switching branches is non-destructive — the stale
        // branch retains every byte of user work, and the session either
        // picks up on the right branch or restarts naturally. The
        // `audit_due` gate upstream already rate-limits how often this path
        // runs, so the cost of letting it run while Working is bounded.

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.exists() {
            return Ok(());
        }

        let current_branch = match current_worktree_branch(&worktree_dir) {
            Ok(branch) => branch,
            Err(_) => return Ok(()),
        };
        let mismatch = ClaimedTaskBranchMismatch {
            task_id: task.id,
            expected_branch: authoritative_task_branch(engineer, task),
            current_branch,
        };
        if mismatch.current_branch == mismatch.expected_branch {
            self.retry_counts.remove(&format!(
                "{ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX}{engineer}::{}",
                task.id
            ));
            return Ok(());
        }

        if mismatch.current_branch == "HEAD" {
            self.handle_orphan_branch_mismatch_failure(engineer, task, &mismatch)?;
            return self.alert_claimed_task_branch_mismatch(
                engineer,
                &mismatch,
                Some(&format_branch_recovery_blocker(
                    "worktree is in detached HEAD state",
                )),
            );
        }

        // Preserve any dirty changes as an auto-save commit on the current
        // branch BEFORE switching. This lets the recovery path work on lanes
        // that have in-flight engineer work, instead of refusing to act.
        // The existing work is preserved on the wrong branch — the engineer
        // can cherry-pick it to the correct branch after recovery lands them
        // on the expected lane.
        if crate::team::task_loop::worktree_has_user_changes(&worktree_dir)? {
            let auto_save_message = format!(
                "wip: auto-save before branch recovery from {} to {}",
                mismatch.current_branch, mismatch.expected_branch
            );
            match crate::worktree::ops::preserve_with_commit(
                &worktree_dir,
                &auto_save_message,
                Duration::from_secs(10),
                "state-reconciliation/branch-recovery",
            ) {
                Ok(true) => {
                    self.record_orchestrator_action(format!(
                        "state reconciliation: auto-saved dirty worktree for {} on branch '{}' before recovery to '{}'",
                        engineer, mismatch.current_branch, mismatch.expected_branch
                    ));
                }
                Ok(false) => {
                    // worktree_has_user_changes said dirty but preserve saw
                    // nothing to save; fall through to checkout.
                }
                Err(error) => {
                    self.handle_orphan_branch_mismatch_failure(engineer, task, &mismatch)?;
                    return self.alert_claimed_task_branch_mismatch(
                        engineer,
                        &mismatch,
                        Some(&format_branch_recovery_blocker(&format!(
                            "failed to auto-save dirty worktree before recovery: {error}"
                        ))),
                    );
                }
            }
        }

        let switch_result = if crate::team::git_cmd::show_ref_exists(
            &self.config.project_root,
            &mismatch.expected_branch,
        )
        .map_err(|error| anyhow::anyhow!("failed to inspect expected branch: {error}"))?
        {
            crate::team::git_cmd::run_git(&worktree_dir, &["checkout", &mismatch.expected_branch])
                .map(|_| ())
                .map_err(|error| anyhow::anyhow!("failed to checkout expected branch: {error}"))
        } else {
            crate::team::git_cmd::checkout_new_branch(
                &worktree_dir,
                &mismatch.expected_branch,
                self.config.team_config.trunk_branch(),
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to create expected branch from {}: {error}",
                    self.config.team_config.trunk_branch()
                )
            })
        };

        match switch_result {
            Ok(()) => {
                self.retry_counts.remove(&format!(
                    "{ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX}{engineer}::{}",
                    task.id
                ));
                self.record_state_reconciliation(Some(engineer), Some(task.id), "branch_repair");
                self.record_orchestrator_action(format!(
                    "state reconciliation: repaired {} from branch '{}' to '{}' for task #{}",
                    engineer, mismatch.current_branch, mismatch.expected_branch, task.id
                ));
                Ok(())
            }
            Err(error) => {
                self.handle_orphan_branch_mismatch_failure(engineer, task, &mismatch)?;
                self.alert_claimed_task_branch_mismatch(
                    engineer,
                    &mismatch,
                    Some(&format_branch_recovery_blocker(&error.to_string())),
                )
            }
        }
    }

    fn handle_orphan_branch_mismatch_failure(
        &mut self,
        engineer: &str,
        task: &crate::task::Task,
        mismatch: &ClaimedTaskBranchMismatch,
    ) -> Result<()> {
        if task.status != "in-progress" {
            return Ok(());
        }

        let retry_key = format!(
            "{ORPHAN_BRANCH_MISMATCH_RETRY_KEY_PREFIX}{engineer}::{}",
            task.id
        );
        let attempts = self.retry_counts.entry(retry_key.clone()).or_insert(0);
        *attempts += 1;
        let max_attempts = orphan_branch_mismatch_max_attempts();
        if *attempts < max_attempts {
            return Ok(());
        }

        warn!(
            task_id = task.id,
            engineer,
            current_branch = %mismatch.current_branch,
            expected_branch = %mismatch.expected_branch,
            "persistent orphaned in-progress branch mismatch exceeded retry limit; moving task back to todo"
        );
        let board_dir = self.board_dir();
        let _ = crate::team::task_cmd::transition_task_with_attribution(
            &board_dir,
            task.id,
            "todo",
            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                "daemon.automation.branch_mismatch_recovery",
            ),
        );
        let _ = crate::team::task_cmd::unclaim_task(&board_dir, task.id);
        self.clear_active_task(engineer);
        self.retry_counts.remove(&retry_key);
        self.record_state_reconciliation(
            Some(engineer),
            Some(task.id),
            "orphan_branch_mismatch_requeued",
        );
        self.record_orchestrator_action(format!(
            "state reconciliation: requeued orphaned task #{} for {} after {} branch-mismatch recovery attempt(s)",
            task.id, engineer, max_attempts
        ));
        Ok(())
    }
}
