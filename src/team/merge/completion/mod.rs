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
mod verify_job;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::info;

use crate::task::load_tasks_from_dir;
use crate::team::daemon::completion_checks::VerificationRunResult;
use crate::team::daemon::{MergeOrigin, TeamDaemon};
use crate::team::events::TeamEvent;
use crate::team::task_loop::current_worktree_branch;
use crate::team::verification_state::VerificationState;

pub(crate) use execute::{
    DeferredDirectMerge, append_merge_learning, enqueue_reviewed_merge,
    poll_deferred_direct_merges, record_merge_test_timing, restore_deferred_direct_merges,
};
pub(crate) use multi_repo::multi_repo_changed_paths;
pub(crate) use verify_job::{AwaitingVeto, VerificationJob, poll_verification_jobs};

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
    if verify_job::verification_already_running(daemon, engineer, task_id) {
        return Ok(());
    }
    if !daemon.member_uses_worktrees(engineer) {
        return Ok(());
    }

    let board_dir = daemon.board_dir();
    if gates::release_terminal_task(daemon, engineer, task_id, &board_dir) {
        return Ok(());
    }

    if !daemon.completion_claim_is_current(engineer, task_id)? {
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
        verify_job::begin_verification(daemon, engineer, task_id, &verification_policy);

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

    start_completion_veto(daemon, engineer, &ctx);

    if run_kind.is_some() {
        if ctx.records_test_evidence {
            ctx.verification_state.begin_iteration();
        }
        verify_job::start_verification_job(daemon, engineer, ctx, test_command);
        return Ok(());
    }

    let verification_run = verify_job::skipped_verification_run(daemon, engineer, &mut ctx);
    finish_or_await_veto(daemon, engineer, ctx, verification_run, 0)
}

/// Ask Jev whether this completion is real, in parallel with verification.
/// No-op unless the completion path is enabled.
fn start_completion_veto(daemon: &mut TeamDaemon, engineer: &str, ctx: &CompletionContext) {
    if !daemon.decision_path_configured(crate::team::config::DecisionPath::Completion) {
        return;
    }
    daemon.start_completion_decision(engineer, ctx.task_id, &ctx.task_text(), &ctx.diff_summary());
}

impl CompletionContext {
    /// The raw task file, for decision input; empty when unreadable.
    fn task_text(&self) -> String {
        crate::team::task_cmd::find_task_path(&self.board_dir, self.task_id)
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
    }

    /// Diff counts plus the diff stat, never the full diff.
    fn diff_summary(&self) -> String {
        let diff = &self.diff;
        format!(
            "commits: {}, files changed: {}, code files changed: {}\n{}",
            diff.total_commits,
            diff.files_changed,
            diff.code_files_changed,
            diff.diff_stat_or_empty()
        )
    }
}

/// Finish a verified completion, or park it while its Jev veto is still
/// pending. Parked completions resume from the poll loop once the decision
/// lands or the completion timeout passes; only a passing completion with
/// full evidence can be vetoed, so every other one finishes now.
fn finish_or_await_veto(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: CompletionContext,
    verification_run: VerificationRunResult,
    test_duration_ms: u64,
) -> Result<()> {
    let vetoable = verification_run.recovery.is_none()
        && verification_run.passed
        && gates::has_required_evidence(daemon, engineer, &ctx);
    if vetoable && daemon.completion_decision_pending(ctx.task_id) {
        verify_job::await_veto(daemon, engineer, ctx, verification_run, test_duration_ms);
        return Ok(());
    }
    finish_engineer_completion(daemon, engineer, ctx, verification_run, test_duration_ms)
}

/// The rejection a cached Jev veto imposes on a completion that passed every
/// hard gate: `NarrationOnly` when the response only narrates, otherwise
/// `InsufficientEvidence`.
fn jev_veto(daemon: &TeamDaemon, task_id: u32) -> Option<gates::Rejection> {
    let outcome = daemon.completion_veto(task_id)?;
    Some(if outcome.narration_only {
        gates::Rejection::NarrationOnly
    } else {
        gates::Rejection::InsufficientEvidence
    })
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
    // A background result must not reclaim a task blocked or reassigned mid-run.
    if daemon.active_task_id(engineer) != Some(ctx.task_id)
        || !daemon.completion_claim_is_current(engineer, ctx.task_id)?
    {
        return Ok(());
    }

    if let Some(recovery) = verification_run.recovery.as_ref() {
        return verify_job::handle_branch_recovery(
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
    if let Some(rejection) = jev_veto(daemon, ctx.task_id) {
        info!(
            engineer,
            task_id = ctx.task_id,
            reason = rejection.label(),
            "Jev vetoed a completion that passed the hard gates"
        );
        daemon.emit_event(TeamEvent::completion_vetoed(
            &ctx.task_id.to_string(),
            rejection.label(),
        ));
        return gates::reject_completion_as(daemon, engineer, ctx, &verification_run, rejection);
    }

    verify_job::complete_verification(daemon, engineer, &mut ctx, &verification_run)?;
    let task_branch = if daemon.is_multi_repo {
        multi_repo::multi_repo_task_branch(
            &ctx.worktree_dir,
            &daemon.sub_repo_names,
            &ctx.trunk_branch,
        )?
    } else {
        current_worktree_branch(&ctx.worktree_dir)?
    };
    verify_job::record_test_telemetry(daemon, engineer, &ctx, &verification_run)?;

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
    verify_job::wait_for_verification_jobs(daemon)
}
