//! Engineer completion handling — the daemon's entry point when an engineer
//! reports a task as done.
//!
//! [`handle_engineer_completion`] gathers the evidence and starts
//! verification; [`finish_engineer_completion`] runs once the verification
//! result is known and routes the task. Each step lives with its concern:
//!
//! - [`gates`]: diff evidence, the scope fence, and rejecting completions
//!   without real work or with failing tests.
//! - [`verification`]: the background verification run and its snapshots.
//! - [`decision`]: confidence scoring and the auto-merge routing decision.
//! - [`execute`]: handing verified work to the merge queue, and the direct
//!   multi-repo merge that never blocks the poll loop.
//! - [`multi_repo`]: multi-repo preflight, merge, and rollback.
//! - [`messaging`]: retry and escalation message text.

mod decision;
mod execute;
mod gates;
mod messaging;
mod multi_repo;
mod verification;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::task::load_tasks_from_dir;
use crate::team::daemon::verification::VerificationRunResult;
use crate::team::daemon::{MergeOrigin, TeamDaemon};
use crate::team::task_loop::current_worktree_branch;
use crate::team::verification::VerificationState;

pub(crate) use execute::{
    DeferredDirectMerge, append_merge_learning, enqueue_reviewed_merge,
    poll_deferred_direct_merges, record_merge_test_timing,
};
pub(crate) use verification::{VerificationJob, poll_verification_jobs};

use decision::MergeRoute;
use gates::DiffEvidence;

/// Everything about a completion that the verification continuation needs,
/// captured before the verification command is started.
pub(crate) struct CompletionContext {
    task_id: u32,
    worktree_dir: PathBuf,
    /// Worktree HEAD when verification started; `None` for multi-repo.
    worktree_head: Option<String>,
    trunk_branch: String,
    board_dir: PathBuf,
    manager_name: Option<String>,
    diff: DiffEvidence,
    verification_policy: crate::team::config::VerificationPolicy,
    verification_state: VerificationState,
    /// True for a real test run whose result is recorded as test evidence.
    records_test_evidence: bool,
}

/// A completion that passed verification, ready to be routed.
struct VerifiedCompletion<'a> {
    engineer: &'a str,
    ctx: &'a CompletionContext,
    task_branch: &'a str,
    verification_run: &'a VerificationRunResult,
    test_duration_ms: u64,
}

pub(crate) fn handle_engineer_completion(daemon: &mut TeamDaemon, engineer: &str) -> Result<()> {
    let Some(task_id) = daemon.active_task_id(engineer) else {
        return Ok(());
    };
    if verification::verification_already_running(daemon, engineer, task_id) {
        return Ok(());
    }
    if !daemon.member_uses_worktrees(engineer) {
        return Ok(());
    }

    let board_dir = daemon.board_dir();
    if gates::release_terminal_task(daemon, engineer, task_id, &board_dir) {
        return Ok(());
    }

    let worktree_dir = daemon.worktree_dir(engineer);
    let trunk_branch = daemon.config.team_config.trunk_branch().to_string();
    let manager_name = daemon.manager_name(engineer);
    let diff = DiffEvidence::collect(daemon, &worktree_dir, &trunk_branch)?;
    let verification_policy = daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .clone();
    let test_command = verification_policy.test_command.clone().or_else(|| {
        daemon
            .config
            .team_config
            .workflow_policy
            .test_command
            .clone()
    });
    let mut verification_state =
        verification::begin_verification(daemon, engineer, task_id, &verification_policy);

    let scope_fence =
        gates::enforce_scope_fence(daemon, engineer, task_id, &worktree_dir, &trunk_branch)?;
    diff.record_evidence(daemon, engineer, task_id, &mut verification_state);

    // Decide whether this completion needs the (possibly slow) verification
    // command. `Some(true)` = a real test run that records test evidence;
    // `Some(false)` = scope-fence checks that may still reach the test
    // command; `None` = tests skipped by policy.
    let run_kind = match scope_fence.as_ref() {
        Some(fence) if !fence.ack_present || !fence.out_of_scope_files.is_empty() => Some(false),
        _ if verification_policy.auto_run_tests => Some(true),
        _ => None,
    };
    let worktree_head = if daemon.is_multi_repo {
        None
    } else {
        super::git_ops::head_commit(&worktree_dir).ok()
    };
    let mut ctx = CompletionContext {
        task_id,
        worktree_dir,
        worktree_head,
        trunk_branch,
        board_dir,
        manager_name,
        diff,
        verification_policy,
        verification_state,
        records_test_evidence: run_kind == Some(true),
    };

    if run_kind.is_some() {
        if ctx.records_test_evidence {
            ctx.verification_state.begin_iteration();
        }
        verification::start_verification_job(daemon, engineer, ctx, test_command);
        return Ok(());
    }

    let verification_run = verification::skipped_verification_run(daemon, engineer, &mut ctx);
    finish_engineer_completion(daemon, engineer, ctx, verification_run, 0)
}

/// Second half of completion handling: runs once the verification result is
/// known (immediately when tests are skipped, otherwise from the poll loop).
fn finish_engineer_completion(
    daemon: &mut TeamDaemon,
    engineer: &str,
    mut ctx: CompletionContext,
    verification_run: VerificationRunResult,
    test_duration_ms: u64,
) -> Result<()> {
    if let Some(recovery) = verification_run.recovery.as_ref() {
        return verification::handle_branch_recovery(
            daemon,
            engineer,
            ctx,
            recovery,
            &verification_run,
        );
    }

    let has_required_evidence = gates::has_required_evidence(daemon, engineer, &ctx);
    if !has_required_evidence || !verification_run.passed {
        return gates::reject_completion(
            daemon,
            engineer,
            ctx,
            &verification_run,
            has_required_evidence,
        );
    }

    verification::complete_verification(daemon, engineer, &mut ctx, &verification_run)?;
    let task_branch = if daemon.is_multi_repo {
        multi_repo::multi_repo_task_branch(
            &ctx.worktree_dir,
            &daemon.sub_repo_names,
            &ctx.trunk_branch,
        )?
    } else {
        current_worktree_branch(&ctx.worktree_dir)?
    };
    verification::record_test_telemetry(daemon, engineer, &ctx, &verification_run)?;

    let verified = VerifiedCompletion {
        engineer,
        ctx: &ctx,
        task_branch: &task_branch,
        verification_run: &verification_run,
        test_duration_ms,
    };
    match decision::decide_merge_route(daemon, &verified) {
        MergeRoute::OverrideManualReview => decision::route_override_to_review(daemon, &verified),
        MergeRoute::ManualReview(decision) => {
            decision::route_to_manual_review(daemon, &verified, &decision)
        }
        MergeRoute::AutoMerge(stats) => {
            execute::enqueue_verified_merge(daemon, &verified, stats, MergeOrigin::AutoMerge)
        }
        MergeRoute::Direct(stats) => execute::merge_directly(daemon, &verified, stats),
    }
}

/// Current board status string for a task, if it can be loaded.
fn task_status(board_dir: &Path, task_id: u32) -> Option<String> {
    load_tasks_from_dir(&board_dir.join("tasks"))
        .ok()?
        .into_iter()
        .find(|task| task.id == task_id)
        .map(|task| task.status)
}

/// Test helper: completion handling as it behaved when verification was
/// synchronous — start it, then pump the poll until the result is applied.
#[cfg(test)]
pub(crate) fn handle_engineer_completion_and_wait(
    daemon: &mut TeamDaemon,
    engineer: &str,
) -> Result<()> {
    handle_engineer_completion(daemon, engineer)?;
    verification::wait_for_verification_jobs(daemon)
}
