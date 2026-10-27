//! Completion gates: the diff evidence a completion must carry, the scope
//! fence, and rejecting completions that lack real work or fail tests.

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

use crate::team::daemon::TeamDaemon;
use crate::team::daemon::verification::{
    ScopeFenceCheck, VerificationRunResult, inspect_scope_fence,
};
use crate::team::telemetry_db;
use crate::team::verification::{EvidenceKind, VerificationPhase};

use super::super::git_ops::{
    code_files_changed_from_trunk, commits_ahead_of_trunk, diff_stat_from_trunk,
    files_changed_from_trunk, revert_out_of_scope_files,
};
use super::messaging::{
    RejectionEscalation, deliver_verification_retry, rejection_block_reason, rejection_headline,
    rejection_manager_message, structured_failure_details, verification_fix_message,
};
use super::multi_repo::{
    multi_repo_code_files_changed_from_trunk, multi_repo_commits_ahead_of_trunk,
    multi_repo_diff_stat_from_trunk, multi_repo_files_changed_from_trunk,
    revert_multi_repo_out_of_scope_files,
};
use super::verification::{
    persist_verification_snapshot, record_verification_evidence, transition_verification_phase,
};
use super::{CompletionContext, task_status};

/// What the engineer's branch changed relative to trunk.
#[derive(Debug, Clone, Default)]
pub(super) struct DiffEvidence {
    pub total_commits: u32,
    pub diff_stat: String,
    pub files_changed: u32,
    pub code_files_changed: u32,
}

impl DiffEvidence {
    pub(super) fn collect(
        daemon: &TeamDaemon,
        worktree_dir: &Path,
        trunk_branch: &str,
    ) -> Result<Self> {
        if daemon.is_multi_repo {
            let repos = &daemon.sub_repo_names;
            return Ok(Self {
                total_commits: multi_repo_commits_ahead_of_trunk(
                    worktree_dir,
                    repos,
                    trunk_branch,
                )?,
                diff_stat: multi_repo_diff_stat_from_trunk(worktree_dir, repos, trunk_branch)?,
                files_changed: multi_repo_files_changed_from_trunk(
                    worktree_dir,
                    repos,
                    trunk_branch,
                )?,
                code_files_changed: multi_repo_code_files_changed_from_trunk(
                    worktree_dir,
                    repos,
                    trunk_branch,
                )?,
            });
        }
        Ok(Self {
            total_commits: commits_ahead_of_trunk(worktree_dir, trunk_branch)?,
            diff_stat: diff_stat_from_trunk(worktree_dir, trunk_branch)?,
            files_changed: files_changed_from_trunk(worktree_dir, trunk_branch)?,
            code_files_changed: code_files_changed_from_trunk(worktree_dir, trunk_branch)?,
        })
    }

    /// Record the non-zero counts as verification evidence.
    pub(super) fn record_evidence(
        &self,
        daemon: &mut TeamDaemon,
        engineer: &str,
        task_id: u32,
        state: &mut crate::team::verification::VerificationState,
    ) {
        let counts = [
            (
                self.total_commits,
                EvidenceKind::CommitsAhead,
                "commits_ahead",
            ),
            (
                self.files_changed,
                EvidenceKind::FilesChanged,
                "files_changed",
            ),
            (
                self.code_files_changed,
                EvidenceKind::CodeFilesChanged,
                "code_files_changed",
            ),
        ];
        for (count, kind, label) in counts {
            if count > 0 {
                record_verification_evidence(
                    daemon,
                    engineer,
                    task_id,
                    state,
                    kind,
                    format!("{label}={count}"),
                );
            }
        }
    }

    pub(super) fn diff_stat_or_empty(&self) -> String {
        if self.diff_stat.is_empty() {
            "(empty)".to_string()
        } else {
            self.diff_stat.clone()
        }
    }
}

/// Why a completion was rejected. The variants are checked in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Rejection {
    /// No commits ahead of trunk.
    NoCommits,
    /// Commits exist but change no code (docs-only, empty commits).
    NarrationOnly,
    /// Some other evidence requirement was not met.
    InsufficientEvidence,
    /// The evidence is fine but the test command failed.
    TestsFailed,
}

impl Rejection {
    fn classify(has_required_evidence: bool, diff: &DiffEvidence) -> Self {
        if has_required_evidence {
            Self::TestsFailed
        } else if diff.total_commits == 0 {
            Self::NoCommits
        } else if diff.code_files_changed == 0 {
            Self::NarrationOnly
        } else {
            Self::InsufficientEvidence
        }
    }
}

/// A completion for a task that is already done or archived only clears the
/// stale assignment. Returns true when that happened.
pub(super) fn release_terminal_task(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    board_dir: &Path,
) -> bool {
    let Some(status) = task_status(board_dir, task_id) else {
        return false;
    };
    if !matches!(status.as_str(), "done" | "archived") {
        return false;
    }
    info!(
        engineer,
        task_id, status, "ignoring completion: task is already terminal; clearing stale assignment"
    );
    daemon.retry_counts.remove(engineer);
    daemon.verification.engineers.remove(engineer);
    daemon
        .verification
        .narration_rejection_counts
        .remove(&task_id);
    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    true
}

/// Inspect the scope fence and revert any out-of-scope files the engineer
/// touched. Returns the fence check for the verification decision.
pub(super) fn enforce_scope_fence(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    worktree_dir: &Path,
    trunk_branch: &str,
) -> Result<Option<ScopeFenceCheck>> {
    let scope_fence = inspect_scope_fence(worktree_dir)?;
    let Some(fence) = scope_fence.as_ref() else {
        return Ok(scope_fence);
    };
    if fence.out_of_scope_files.is_empty() {
        return Ok(scope_fence);
    }
    let violation_list = fence.out_of_scope_files.join(", ");
    warn!(
        engineer,
        task_id,
        files = %violation_list,
        "completion rejected: engineer modified protected files outside task scope"
    );
    daemon.record_orchestrator_action(format!(
        "scope violation: {engineer} modified fenced files outside declared scope: {violation_list} — reverting out-of-scope changes"
    ));
    if daemon.is_multi_repo {
        revert_multi_repo_out_of_scope_files(
            worktree_dir,
            &daemon.sub_repo_names,
            &fence.out_of_scope_files,
            trunk_branch,
        )?;
    } else {
        revert_out_of_scope_files(worktree_dir, &fence.out_of_scope_files, trunk_branch)?;
    }
    daemon.emit_event(crate::team::events::TeamEvent::scope_fence_violation(
        engineer,
        task_id,
        &format!("reclaim_requested=true out_of_scope_files={violation_list}"),
    ));
    Ok(scope_fence)
}

/// Whether the diff carries the evidence the verification policy requires:
/// commits plus code changes (or any file changes for docs-only roles).
pub(super) fn has_required_evidence(
    daemon: &TeamDaemon,
    engineer: &str,
    ctx: &CompletionContext,
) -> bool {
    if !ctx.verification_policy.require_evidence {
        return true;
    }
    let docs_only_evidence = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == engineer)
        .and_then(|member| daemon.config.team_config.role_def(&member.role_name))
        .is_some_and(|role| role.docs_only_evidence);
    let diff = &ctx.diff;
    diff.total_commits > 0
        && (diff.code_files_changed > 0 || (docs_only_evidence && diff.files_changed > 0))
}

/// Reject a completion that lacks evidence or failed verification: send the
/// engineer a fix message, and escalate (blocking the task) once the retry
/// budget — or the narration-only limit — is exhausted.
pub(super) fn reject_completion(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: CompletionContext,
    verification_run: &VerificationRunResult,
    has_required_evidence: bool,
) -> Result<()> {
    let CompletionContext {
        task_id,
        trunk_branch,
        board_dir,
        manager_name,
        diff,
        mut verification_state,
        ..
    } = ctx;
    let verification_results = &verification_run.results;
    if !verification_run.passed
        && let Some(conn) = &daemon.telemetry_db
    {
        telemetry_db::record_test_results(conn, task_id, engineer, verification_results, &[])?;
    }
    let rejection = Rejection::classify(has_required_evidence, &diff);
    // A narration-only completion is one that DID land real commits but
    // produced no code changes. A zero-commit attempt is a distinct failure
    // mode and must not inflate the narration counter (#635).
    let narration_count = if rejection == Rejection::NarrationOnly {
        let count = {
            let entry = daemon
                .verification
                .narration_rejection_counts
                .entry(task_id)
                .or_insert(0);
            *entry += 1;
            *entry
        };
        daemon.record_narration_rejection(engineer, task_id, count);
        count
    } else {
        0
    };
    // Zero-commit attempts escalate at max_iterations like any other failure.
    // Without a cap, a member whose work already landed on trunk (so it can
    // never get ahead of it again) is rejected forever.
    let should_escalate = if rejection == Rejection::NarrationOnly {
        narration_count >= 2
    } else {
        verification_state.reached_max_iterations()
    };
    let next_phase = if should_escalate {
        VerificationPhase::Failed
    } else {
        VerificationPhase::Fixing
    };
    transition_verification_phase(
        daemon,
        engineer,
        task_id,
        &mut verification_state,
        next_phase,
    );
    persist_verification_snapshot(
        daemon.project_root(),
        &board_dir,
        task_id,
        engineer,
        &verification_state,
        verification_run,
    )?;

    let headline = rejection_headline(rejection, &diff, &trunk_branch, verification_results);
    let engineer_message = verification_fix_message(
        &verification_state,
        &headline,
        &structured_failure_details(verification_results),
        &verification_run.file_paths,
        &verification_run.output,
    );
    deliver_verification_retry(daemon, engineer, task_id, &engineer_message)?;

    if !should_escalate {
        daemon
            .verification
            .engineers
            .insert(engineer.to_string(), verification_state);
        return Ok(());
    }

    if let Some(ref manager_name) = manager_name {
        daemon.record_verification_max_iterations_reached(
            engineer,
            task_id,
            verification_state.iteration,
            manager_name,
        );
        let manager_message = rejection_manager_message(
            rejection,
            &RejectionEscalation {
                engineer,
                task_id,
                trunk_branch: &trunk_branch,
                state: &verification_state,
                narration_count,
                diff: &diff,
                verification_run,
            },
        );
        daemon.queue_message("daemon", manager_name, &manager_message)?;
    }

    let block_reason = rejection_block_reason(
        rejection,
        &verification_state,
        &trunk_branch,
        verification_results,
    );
    daemon.record_task_escalated(engineer, task_id.to_string(), Some("verification_failed"));
    block_escalated_task(&board_dir, task_id, engineer, block_reason);
    daemon.retry_counts.remove(engineer);
    daemon.verification.engineers.remove(engineer);
    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    daemon
        .verification
        .narration_rejection_counts
        .remove(&task_id);
    Ok(())
}

/// Move an escalated task to blocked with `block_reason`. A failed transition
/// (task already Done, or still Backlog) must not leave the verification
/// state behind, or the same escalation would fire on every completion, so
/// failures are only logged. Backlog cannot block directly, so it goes via
/// todo.
fn block_escalated_task(board_dir: &Path, task_id: u32, engineer: &str, block_reason: String) {
    let transition = |status: &str| {
        crate::team::task_cmd::transition_task_with_attribution(
            board_dir,
            task_id,
            status,
            crate::team::task_cmd::StatusTransitionAttribution::daemon(
                "daemon.merge.completion.verification",
            ),
        )
    };
    let blocked = if task_status(board_dir, task_id).as_deref() == Some("backlog") {
        transition("todo").and_then(|()| transition("blocked"))
    } else {
        transition("blocked")
    };
    match blocked {
        Ok(()) => {
            let mut blocked_fields = std::collections::HashMap::new();
            blocked_fields.insert("blocked_on".to_string(), block_reason);
            if let Err(error) =
                crate::team::task_cmd::cmd_update(board_dir, task_id, blocked_fields)
            {
                warn!(engineer, task_id, error = %error, "failed to record blocked_on");
            }
        }
        Err(error) => warn!(
            engineer,
            task_id,
            error = %error,
            "could not block task after verification escalation; clearing assignment anyway"
        ),
    }
}
