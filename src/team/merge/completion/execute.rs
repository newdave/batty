//! Merge execution for verified completions.
//!
//! Single-repo merges always go through the daemon merge queue, which takes
//! the merge lock without blocking, serializes merges, and runs post-merge
//! verification (reverting a merge that breaks trunk). The queue cannot merge
//! multi-repo projects yet, so those merge here directly — but never block
//! the poll loop: when the merge lock is busy, a queue merge is still being
//! verified, or the main smoke check is running, the merge is deferred (the
//! task waits in review) and retried by [`poll_deferred_direct_merges`].

use std::time::Instant;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use crate::task::load_tasks_from_dir;
use crate::team::artifact::append_test_timing_record;
use crate::team::daemon::{MergeOrigin, MergeRequest, TeamDaemon};
use crate::team::task_loop::{current_worktree_branch, read_task_title};
use crate::team::test_results::TestResults;

use super::super::git_ops::now_unix;
use super::super::lock::{MergeLock, MergeMode, MergeOutcome, infer_merge_mode_from_failure};
use super::VerifiedCompletion;
use super::decision::{MergeStats, move_task_to_review, move_to_review_with_packet};
use super::multi_repo::merge_multi_repo_engineer_branch;

/// Move a verified completion to review with its completion packet and hand
/// it to the merge queue.
pub(super) fn enqueue_verified_merge(
    daemon: &mut TeamDaemon,
    verified: &VerifiedCompletion<'_>,
    stats: MergeStats,
    origin: MergeOrigin,
) -> Result<()> {
    if !move_to_review_with_packet(daemon, verified)? {
        return Ok(());
    }
    let ctx = verified.ctx;
    daemon.enqueue_merge_request(MergeRequest {
        task_id: ctx.task_id,
        engineer: verified.engineer.to_string(),
        branch: verified.task_branch.to_string(),
        worktree_dir: ctx.worktree_dir.clone(),
        queued_at: Instant::now(),
        test_passed: true,
        should_post_merge_verify: ctx.verification_policy.auto_run_tests,
        test_duration_ms: verified.test_duration_ms,
        confidence: stats.confidence,
        files_changed: stats.files_changed,
        lines_changed: stats.lines_changed,
        origin,
    });
    Ok(())
}

/// Merge a verified completion while auto-merge is off (no review gate).
pub(super) fn merge_directly(
    daemon: &mut TeamDaemon,
    verified: &VerifiedCompletion<'_>,
    stats: MergeStats,
) -> Result<()> {
    if !daemon.is_multi_repo {
        return enqueue_verified_merge(daemon, verified, stats, MergeOrigin::DirectCompletion);
    }

    let ctx = verified.ctx;
    let merge = DeferredDirectMerge {
        engineer: verified.engineer.to_string(),
        task_id: ctx.task_id,
        task_branch: verified.task_branch.to_string(),
        test_duration_ms: verified.test_duration_ms,
    };
    let DirectMergeAttempt::Deferred(reason) = attempt_direct_merge(daemon, &merge)? else {
        return Ok(());
    };
    if !move_task_to_review(
        daemon,
        &ctx.board_dir,
        ctx.task_id,
        ctx.manager_name.as_deref(),
        verified.engineer,
    )? {
        return Ok(());
    }
    daemon.record_orchestrator_action(format!(
        "completion merge: deferred task #{} ({reason}); it waits in review and is retried on a later tick",
        ctx.task_id
    ));
    let deferred = &mut daemon.merge.deferred_direct;
    deferred.retain(|pending| pending.task_id != merge.task_id);
    deferred.push(merge);
    Ok(())
}

/// Queue a merge for a task that is already in review and whose tests just
/// passed (chat `merge` command). The completion packet is refreshed so the
/// queue's packet check sees the verified commit.
pub(crate) fn enqueue_reviewed_merge(
    daemon: &mut TeamDaemon,
    task_id: u32,
    engineer: &str,
    test_results: TestResults,
    test_duration_ms: u64,
) -> Result<()> {
    if daemon.is_multi_repo {
        bail!("merging from chat is not supported for multi-repo projects");
    }
    let worktree_dir = daemon.worktree_dir(engineer);
    let branch = current_worktree_branch(&worktree_dir)?;
    let board_dir = daemon.board_dir();
    super::decision::record_completion_packet_metadata(
        daemon.project_root(),
        &board_dir,
        task_id,
        &branch,
        &worktree_dir,
        true,
        Some(test_results),
    )?;
    let should_post_merge_verify = daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .auto_run_tests;
    daemon.enqueue_merge_request(MergeRequest {
        task_id,
        engineer: engineer.to_string(),
        branch,
        worktree_dir,
        queued_at: Instant::now(),
        test_passed: true,
        should_post_merge_verify,
        test_duration_ms,
        confidence: 0.0,
        files_changed: 0,
        lines_changed: 0,
        origin: MergeOrigin::Chat,
    });
    Ok(())
}

/// A multi-repo direct merge waiting for the merge lock (or for a queue
/// merge / main smoke check to finish). Held in daemon merge state and
/// retried every poll tick; not persisted across restarts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeferredDirectMerge {
    pub(super) engineer: String,
    pub(super) task_id: u32,
    pub(super) task_branch: String,
    pub(super) test_duration_ms: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum DirectMergeAttempt {
    /// The merge ran; its outcome (success, conflict, failure) was handled.
    Finished,
    /// Nothing was merged; retry on a later tick.
    Deferred(&'static str),
}

/// Why a direct merge must not run right now, if anything.
fn direct_merge_blocker(daemon: &TeamDaemon) -> Option<&'static str> {
    if daemon.merge.queue.is_verifying() {
        // A failed post-merge verification resets trunk to its pre-merge
        // head only if nothing else landed meanwhile.
        return Some("a merge-queue merge is still in post-merge verification");
    }
    if daemon.verification.background_tests.main_smoke_running() {
        return Some("the main smoke check is running on trunk");
    }
    None
}

fn attempt_direct_merge(
    daemon: &mut TeamDaemon,
    merge: &DeferredDirectMerge,
) -> Result<DirectMergeAttempt> {
    if let Some(reason) = direct_merge_blocker(daemon) {
        return Ok(DirectMergeAttempt::Deferred(reason));
    }
    let Some(lock) =
        MergeLock::try_acquire(daemon.project_root()).context("failed to acquire merge lock")?
    else {
        return Ok(DirectMergeAttempt::Deferred(
            "the merge lock is held by another merge",
        ));
    };
    let trunk_branch = daemon.config.team_config.trunk_branch().to_string();
    let outcome = merge_multi_repo_engineer_branch(
        daemon.project_root(),
        &merge.engineer,
        &daemon.sub_repo_names,
        &trunk_branch,
    );
    drop(lock);
    match outcome? {
        MergeOutcome::Success(success) => {
            land_direct_merge(daemon, merge, &trunk_branch, success.mode, success.reason)?
        }
        MergeOutcome::RebaseConflict(conflict_info) => {
            handle_direct_merge_conflict(daemon, merge, &trunk_branch, &conflict_info)?
        }
        MergeOutcome::MergeFailure(merge_info) => {
            handle_direct_merge_failure(daemon, merge, &trunk_branch, &merge_info)?
        }
    }
    Ok(DirectMergeAttempt::Finished)
}

/// Retry deferred direct merges. Called from the daemon poll loop; merges
/// whose task is no longer the engineer's active task are dropped. Returns
/// the first error after attempting every merge.
pub(crate) fn poll_deferred_direct_merges(daemon: &mut TeamDaemon) -> Result<()> {
    if daemon.merge.deferred_direct.is_empty() {
        return Ok(());
    }
    let pending = std::mem::take(&mut daemon.merge.deferred_direct);
    let mut first_error = None;
    for merge in pending {
        if daemon.active_task_id(&merge.engineer) != Some(merge.task_id) {
            info!(
                engineer = %merge.engineer,
                task_id = merge.task_id,
                "dropping deferred direct merge: task is no longer active for this engineer"
            );
            continue;
        }
        match attempt_direct_merge(daemon, &merge) {
            Ok(DirectMergeAttempt::Finished) => {}
            Ok(DirectMergeAttempt::Deferred(reason)) => {
                debug!(
                    task_id = merge.task_id,
                    reason, "direct merge still deferred"
                );
                daemon.merge.deferred_direct.push(merge);
            }
            Err(error) => {
                warn!(
                    engineer = %merge.engineer,
                    task_id = merge.task_id,
                    error = %format!("{error:#}"),
                    "deferred direct merge failed before merging; task stays in review"
                );
                daemon.record_orchestrator_action(format!(
                    "completion merge: deferred merge of task #{} failed ({error:#}); task stays in review",
                    merge.task_id
                ));
                first_error.get_or_insert(error);
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn land_direct_merge(
    daemon: &mut TeamDaemon,
    merge: &DeferredDirectMerge,
    trunk_branch: &str,
    mode: MergeMode,
    merge_reason: Option<String>,
) -> Result<()> {
    let engineer = merge.engineer.as_str();
    let task_id = merge.task_id;
    let board_dir = daemon.board_dir();
    let board_dir_str = board_dir.to_string_lossy().to_string();
    let manager_name = daemon.manager_name(engineer);
    let task_title = read_task_title(&board_dir, task_id);
    if mode == MergeMode::IsolatedIntegration {
        let reason = merge_reason
            .as_deref()
            .unwrap_or("root checkout required isolation");
        daemon.record_orchestrator_action(format!(
            "completion merge: isolated merge for task #{task_id} ({reason})"
        ));
    }

    if let Err(error) = record_merge_test_timing(
        daemon,
        task_id,
        engineer,
        &merge.task_branch,
        merge.test_duration_ms,
    ) {
        warn!(
            engineer,
            task_id,
            error = %error,
            "failed to record merge test timing"
        );
    }
    daemon.record_task_manual_merged(task_id, mode);

    let board_update_ok = daemon.run_kanban_md_nonfatal(
        &[
            "move",
            &task_id.to_string(),
            "done",
            "--claim",
            engineer,
            "--dir",
            &board_dir_str,
        ],
        &format!("move task #{task_id} to done"),
        manager_name
            .as_deref()
            .into_iter()
            .chain(std::iter::once(engineer)),
    );

    if let Some(ref manager_name) = manager_name {
        let msg = format!(
            "[{engineer}] Task #{task_id} completed.\nTitle: {task_title}\nTests: passed\nMerge: success{}{}{}",
            if mode == MergeMode::IsolatedIntegration {
                "\nMerge mode: isolated integration checkout"
            } else {
                ""
            },
            if let Some(reason) = merge_reason.as_deref() {
                format!("\nMerge reason: {reason}")
            } else {
                String::new()
            },
            if board_update_ok {
                ""
            } else {
                "\nBoard: update failed; decide next board action manually."
            }
        );
        daemon.queue_message(engineer, manager_name, &msg)?;
        daemon.mark_member_working(manager_name);

        let rollup = format!(
            "Rollup: Task #{task_id} completed by {engineer}. Tests passed, merged to {trunk_branch}.{}",
            if board_update_ok {
                ""
            } else {
                " Board automation failed; decide manually."
            }
        );
        daemon.notify_reports_to(manager_name, &rollup)?;
    }

    append_merge_learning(daemon, &board_dir, task_id, engineer);
    daemon.clear_active_task(engineer);
    daemon.record_task_completed(engineer, Some(task_id));
    daemon.set_member_idle(engineer);
    Ok(())
}

/// Record a "merged cleanly" learning for the task (best effort).
pub(crate) fn append_merge_learning(
    daemon: &TeamDaemon,
    board_dir: &std::path::Path,
    task_id: u32,
    engineer: &str,
) {
    let Ok(tasks) = load_tasks_from_dir(&board_dir.join("tasks")) else {
        return;
    };
    let Some(task) = tasks.into_iter().find(|task| task.id == task_id) else {
        return;
    };
    let learning_summary = format!(
        "Task #{task_id} merged cleanly after passing verification. Title: {}",
        task.title
    );
    if let Err(error) = crate::team::learnings::append_task_completion_learning(
        daemon.project_root(),
        &task,
        engineer,
        &learning_summary,
    ) {
        warn!(
            engineer,
            task_id,
            error = %error,
            "failed to persist task learning"
        );
    }
}

fn handle_direct_merge_conflict(
    daemon: &mut TeamDaemon,
    merge: &DeferredDirectMerge,
    trunk_branch: &str,
    conflict_info: &str,
) -> Result<()> {
    let engineer = merge.engineer.as_str();
    let task_id = merge.task_id;
    let manager_name = daemon.manager_name(engineer);
    let attempt = daemon.increment_retry(engineer);
    if attempt <= 2 {
        let msg = format!(
            "Merge conflict during rebase onto {trunk_branch} (attempt {attempt}/2). Fix the conflicts in your worktree and try again:\n{conflict_info}"
        );
        daemon.queue_message("batty", engineer, &msg)?;
        daemon.mark_member_working(engineer);
        info!(engineer, attempt, "rebase conflict, sending back for retry");
        return Ok(());
    }

    if let Some(ref manager_name) = manager_name {
        let msg = format!(
            "[{engineer}] task #{task_id} has unresolvable merge conflicts after 2 retries. Escalating.\n{conflict_info}"
        );
        daemon.queue_message(engineer, manager_name, &msg)?;
        daemon.mark_member_working(manager_name);
    }

    daemon.record_task_escalated(engineer, task_id.to_string(), Some("merge_conflict"));

    if let Some(ref manager_name) = manager_name {
        let escalation = format!(
            "ESCALATION: Task #{task_id} assigned to {engineer} has unresolvable merge conflicts. Task blocked on board."
        );
        daemon.notify_reports_to(manager_name, &escalation)?;
    }

    let board_dir_str = daemon.board_dir().to_string_lossy().to_string();
    daemon.run_kanban_md_nonfatal(
        &[
            "edit",
            &task_id.to_string(),
            "--block",
            "merge conflicts after 2 retries",
            "--dir",
            &board_dir_str,
        ],
        &format!("block task #{task_id} after merge conflict retries"),
        manager_name
            .as_deref()
            .into_iter()
            .chain(std::iter::once(engineer)),
    );

    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    Ok(())
}

fn handle_direct_merge_failure(
    daemon: &mut TeamDaemon,
    merge: &DeferredDirectMerge,
    trunk_branch: &str,
    merge_info: &str,
) -> Result<()> {
    let engineer = merge.engineer.as_str();
    let task_id = merge.task_id;
    let manager_name = daemon.manager_name(engineer);
    daemon.record_task_merge_failed(
        engineer,
        task_id,
        infer_merge_mode_from_failure(merge_info),
        merge_info,
    );

    let manager_notice = format!(
        "Task #{task_id} from {engineer} passed tests but could not be merged to {trunk_branch}.\n{merge_info}\nDecide whether to retry the isolated merge path, inspect the integration-worktree failure, or redirect the engineer."
    );
    if let Some(ref manager_name) = manager_name {
        daemon.queue_message("daemon", manager_name, &manager_notice)?;
        daemon.mark_member_working(manager_name);
        daemon.notify_reports_to(manager_name, &manager_notice)?;
    }

    let engineer_notice = format!(
        "Your task passed tests, but Batty could not merge it into {trunk_branch}.\n{merge_info}\nWait for lead direction before making more changes."
    );
    daemon.queue_message("daemon", engineer, &engineer_notice)?;

    daemon.record_task_escalated(engineer, task_id.to_string(), Some("merge_failure"));
    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    warn!(
        engineer,
        task_id,
        error = %merge_info,
        "merge into main failed after passing tests; escalated without exiting daemon"
    );
    Ok(())
}

pub(crate) fn record_merge_test_timing(
    daemon: &mut TeamDaemon,
    task_id: u32,
    engineer: &str,
    task_branch: &str,
    test_duration_ms: u64,
) -> Result<()> {
    let log_path = daemon
        .project_root()
        .join(".batty")
        .join("test_timing.jsonl");
    let record = append_test_timing_record(
        &log_path,
        task_id,
        engineer,
        task_branch,
        now_unix(),
        test_duration_ms,
    )?;

    if record.regression_detected {
        let rolling_average_ms = record.rolling_average_ms.unwrap_or_default();
        let regression_pct = record.regression_pct.unwrap_or_default();
        let reason = format!(
            "runtime_ms={} avg_ms={} pct={}",
            record.duration_ms, rolling_average_ms, regression_pct
        );
        daemon.record_performance_regression(task_id.to_string(), &reason);
        warn!(
            engineer,
            task_id,
            runtime_ms = record.duration_ms,
            rolling_average_ms,
            regression_pct,
            "post-merge test runtime exceeded rolling average"
        );
    }

    Ok(())
}
