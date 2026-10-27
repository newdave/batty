//! Retry, recovery, and escalation messages sent while handling a completion.

use anyhow::Result;
use tracing::warn;

use crate::team::daemon::TeamDaemon;
use crate::team::daemon::verification::{VerificationRecovery, VerificationRunResult};
use crate::team::test_results::TestResults;
use crate::team::verification::VerificationState;

use super::gates::{DiffEvidence, Rejection};
use super::verification::verification_retry_budget;

pub(super) fn verification_fix_message(
    state: &VerificationState,
    headline: &str,
    failures: &[String],
    file_paths: &[String],
    output: &str,
) -> String {
    let mut body = format!(
        "{headline}\nFix attempt {}/{}.\n",
        state.iteration,
        verification_retry_budget(state.max_iterations)
    );
    if !failures.is_empty() {
        body.push_str("Failures:\n");
        for failure in failures.iter().take(8) {
            body.push_str("- ");
            body.push_str(failure);
            body.push('\n');
        }
    }
    if !file_paths.is_empty() {
        body.push_str("Likely files to inspect:\n");
        for path in file_paths.iter().take(8) {
            body.push_str("- ");
            body.push_str(path);
            body.push('\n');
        }
    }
    body.push_str("Latest verification output:\n```\n");
    body.push_str(output.trim());
    body.push_str("\n```\nFix these failures, then report completion again.");
    body
}

fn can_deliver_assignment(daemon: &TeamDaemon, engineer: &str) -> bool {
    (daemon.config.team_config.use_shim && daemon.shim_handles.contains_key(engineer))
        || daemon.config.pane_map.contains_key(engineer)
}

/// Re-dispatch the task with the fix message, falling back to the inbox
/// when the engineer cannot receive an assignment right now.
pub(super) fn deliver_verification_retry(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    message: &str,
) -> Result<()> {
    if can_deliver_assignment(daemon, engineer) {
        match daemon.assign_task_with_task_id_as("batty", engineer, message, Some(task_id)) {
            Ok(_) => {
                daemon.record_orchestrator_action(format!(
                    "verification retry: re-dispatched task #{task_id} to {engineer}"
                ));
                return Ok(());
            }
            Err(error) => {
                warn!(
                    engineer,
                    task_id,
                    error = %error,
                    "verification retry re-dispatch failed; falling back to inbox delivery"
                );
            }
        }
    }

    daemon.queue_message("batty", engineer, message)
}

pub(super) fn verification_recovery_message(recovery: &VerificationRecovery) -> String {
    let preserved = if recovery.preserved_branches.is_empty() {
        "No preserved branch evidence was found.".to_string()
    } else {
        format!(
            "Preserved branch evidence: {}.",
            recovery
                .preserved_branches
                .iter()
                .map(|branch| format!("`{branch}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    format!(
        "{}\n\nRecovery action: switch this lane back to `{}` for task #{} before verifying again. Current branch: `{}`. {preserved}\n\nThis is recorded as `branch_recovery_required`; it did not consume a verification retry.",
        recovery.message,
        recovery.expected_branch,
        recovery.expected_task_id,
        recovery.actual_branch
    )
}

pub(super) fn structured_failure_details(results: &TestResults) -> Vec<String> {
    results
        .failures
        .iter()
        .map(|failure| {
            let mut detail = failure.test_name.clone();
            if let Some(message) = failure
                .message
                .as_deref()
                .filter(|message| !message.is_empty())
            {
                detail.push_str(" (");
                detail.push_str(message);
                if let Some(location) = failure
                    .location
                    .as_deref()
                    .filter(|location| !location.is_empty())
                {
                    detail.push_str(" at ");
                    detail.push_str(location);
                }
                detail.push(')');
            } else if let Some(location) = failure
                .location
                .as_deref()
                .filter(|location| !location.is_empty())
            {
                detail.push_str(" (at ");
                detail.push_str(location);
                detail.push(')');
            }
            detail
        })
        .collect()
}

/// First line of the fix message sent to the engineer for a rejection.
pub(super) fn rejection_headline(
    rejection: Rejection,
    diff: &DiffEvidence,
    trunk_branch: &str,
    results: &TestResults,
) -> String {
    let DiffEvidence {
        total_commits,
        files_changed,
        code_files_changed,
        ..
    } = *diff;
    let diff_stat = diff.diff_stat_or_empty();
    match rejection {
        Rejection::NoCommits => format!(
            "Verification rejected this completion because there are no commits ahead of {trunk_branch}. Run `git add -A`, create a real commit, and inspect `git diff --stat {trunk_branch}..HEAD` before reporting completion again. Commits ahead of {trunk_branch}: {total_commits}. Files changed: {files_changed}. Code files changed: {code_files_changed}. Diff stat: {diff_stat}"
        ),
        Rejection::NarrationOnly => {
            let narration_prefix = if files_changed == 0 {
                "Your previous attempt only produced commentary. Execute the actual commands to make the code changes."
            } else {
                "Verification found no code changes in this completion."
            };
            format!(
                "{narration_prefix} Inspect `git diff --stat {trunk_branch}..HEAD` and make an actual code change before reporting completion again. Commits ahead of {trunk_branch}: {total_commits}. Files changed: {files_changed}. Code files changed: {code_files_changed}. Diff stat: {diff_stat}"
            )
        }
        Rejection::InsufficientEvidence => format!(
            "Verification could not accept this completion: no sufficient task evidence was found. Commits ahead of {trunk_branch}: {total_commits}. Files changed: {files_changed}. Code files changed: {code_files_changed}. Diff stat: {diff_stat}"
        ),
        Rejection::TestsFailed => format!(
            "Verification failed because the test command did not pass. Summary: {}.",
            results.failure_summary()
        ),
    }
}

/// Everything the manager escalation for a rejected completion reports.
pub(super) struct RejectionEscalation<'a> {
    pub engineer: &'a str,
    pub task_id: u32,
    pub trunk_branch: &'a str,
    pub state: &'a VerificationState,
    pub narration_count: u32,
    pub diff: &'a DiffEvidence,
    pub verification_run: &'a VerificationRunResult,
}

/// Message to the manager when a rejected completion escalates.
pub(super) fn rejection_manager_message(
    rejection: Rejection,
    escalation: &RejectionEscalation<'_>,
) -> String {
    let RejectionEscalation {
        engineer,
        task_id,
        trunk_branch,
        state,
        narration_count,
        diff,
        verification_run,
    } = *escalation;
    let results = &verification_run.results;
    match rejection {
        Rejection::NoCommits => format!(
            "[daemon] Engineer {engineer} reported task #{task_id} complete {} times with no commits ahead of {trunk_branch}.\nIts work may already be on {trunk_branch}; if so, mark the task done, otherwise reassign it.\nAttempts: {}/{}",
            state.iteration, state.iteration, state.max_iterations,
        ),
        Rejection::NarrationOnly => format!(
            "[daemon] Engineer {engineer} hit the narration-only quality gate {} times on task #{task_id}.\nReason: completion diff is narration-only.\nAttempts: {}/{}\nDiff stat: {}\nLatest output:\n```\\n{}\\n```",
            narration_count,
            state.iteration,
            state.max_iterations,
            diff.diff_stat_or_empty(),
            verification_run.output.trim()
        ),
        Rejection::TestsFailed => format!(
            "[daemon] Engineer {engineer} hit verification max iterations on task #{task_id}.\nLatest phase: failed\nAttempts: {}/{}\nSummary: {}\nRecent failures:\n{}\nLatest output:\n```\\n{}\\n```",
            state.iteration,
            state.max_iterations,
            results.failure_summary(),
            structured_failure_details(results)
                .iter()
                .take(8)
                .map(|failure| format!("- {failure}"))
                .collect::<Vec<_>>()
                .join("\n"),
            verification_run.output.trim()
        ),
        Rejection::InsufficientEvidence => format!(
            "[daemon] Engineer {engineer} hit verification max iterations on task #{task_id}.\nLatest phase: failed\nAttempts: {}/{}\nDiff stat: {}\nRecent failures:\n{}\nLatest output:\n```\\n{}\\n```",
            state.iteration,
            state.max_iterations,
            diff.diff_stat_or_empty(),
            verification_run
                .failures
                .iter()
                .take(8)
                .map(|failure| format!("- {failure}"))
                .collect::<Vec<_>>()
                .join("\n"),
            verification_run.output.trim()
        ),
    }
}

/// `blocked_on` reason recorded on the board when a rejection escalates.
pub(super) fn rejection_block_reason(
    rejection: Rejection,
    state: &VerificationState,
    trunk_branch: &str,
    results: &TestResults,
) -> String {
    let attempts = state.iteration;
    match rejection {
        Rejection::NoCommits => format!(
            "verification escalation after {attempts} attempts: no commits ahead of {trunk_branch}"
        ),
        Rejection::NarrationOnly => {
            format!("verification escalation after {attempts} attempts: narration-only completion")
        }
        Rejection::TestsFailed => format!(
            "verification escalation after {attempts} attempts: {}",
            results.failure_summary()
        ),
        Rejection::InsufficientEvidence => {
            format!("verification escalation after {attempts} attempts: insufficient task evidence")
        }
    }
}
