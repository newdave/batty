//! Completion verification: the background test run, its result handling,
//! verification-state transitions, and the persisted verification snapshots.

use std::path::Path;
#[cfg(test)]
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::team::artifact::track_artifact;
use crate::team::config::VerificationPolicy;
use crate::team::daemon::TeamDaemon;
use crate::team::daemon::verification::{
    VerificationRecovery, VerificationRunResult, run_automatic_verification_tracked,
};
use crate::team::telemetry_db;
use crate::team::test_job::TestJob;
use crate::team::test_results::{TestFailure, TestResults};
use crate::team::verification::{EvidenceKind, VerificationPhase, VerificationState};

use super::super::git_ops::{branch_head, head_commit};
use super::messaging::verification_recovery_message;
use super::{CompletionContext, finish_engineer_completion, handle_engineer_completion};

/// A completion verification running on a background thread, keyed by
/// engineer in `TeamDaemon::background_tests`.
pub(crate) struct VerificationJob {
    ctx: CompletionContext,
    job: TestJob<VerificationRunResult>,
}

impl VerificationJob {
    pub(crate) fn task_id(&self) -> u32 {
        self.ctx.task_id
    }

    pub(crate) fn kill(&self) {
        self.job.kill();
    }
}

/// True when a verification for `task_id` is already running (a repeated
/// completion is ignored). A running verification for another task is
/// cancelled, since the engineer no longer owns it.
pub(super) fn verification_already_running(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
) -> bool {
    let Some(running) = daemon
        .verification
        .background_tests
        .verification
        .get(engineer)
    else {
        return false;
    };
    if running.task_id() == task_id {
        info!(
            engineer,
            task_id, "completion verification already running; ignoring repeated completion"
        );
        return true;
    }
    info!(
        engineer,
        stale_task_id = running.task_id(),
        task_id,
        "cancelling verification for a task the engineer no longer owns"
    );
    running.kill();
    daemon
        .verification
        .background_tests
        .verification
        .remove(engineer);
    false
}

/// Take (or create) the engineer's verification state and move it to
/// `Verifying` for a new completion attempt.
pub(super) fn begin_verification(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    policy: &VerificationPolicy,
) -> VerificationState {
    let mut state = daemon
        .verification
        .engineers
        .remove(engineer)
        .unwrap_or_else(|| VerificationState::new(policy.max_iterations));
    state.max_iterations = verification_retry_budget(policy.max_iterations);
    state.clear_evidence();
    transition_verification_phase(
        daemon,
        engineer,
        task_id,
        &mut state,
        VerificationPhase::Verifying,
    );
    state
}

pub(super) fn transition_verification_phase(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    state: &mut VerificationState,
    next_phase: VerificationPhase,
) {
    let next_phase_name = format!("{next_phase:?}").to_ascii_lowercase();
    let previous_phase = state.transition(next_phase);
    let previous_phase_name = format!("{previous_phase:?}").to_ascii_lowercase();
    daemon.record_verification_phase_changed(
        engineer,
        task_id,
        &previous_phase_name,
        &next_phase_name,
        state.iteration,
    );
}

pub(super) fn record_verification_evidence(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    state: &mut VerificationState,
    kind: EvidenceKind,
    detail: impl Into<String>,
) {
    let detail = detail.into();
    let kind_name = kind_name(&kind);
    state.record_evidence(kind, detail.clone());
    daemon.record_verification_evidence_collected(engineer, task_id, &kind_name, &detail);
}

fn kind_name(kind: &EvidenceKind) -> String {
    let mut name = String::new();
    for (index, ch) in format!("{kind:?}").chars().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            name.push('_');
        }
        name.push(ch.to_ascii_lowercase());
    }
    name
}

pub(super) fn verification_retry_budget(max_iterations: u32) -> u32 {
    max_iterations.clamp(1, 3)
}

fn verification_outcome_label(phase: &VerificationPhase) -> &'static str {
    match phase {
        VerificationPhase::Fixing => "verification_retry_required",
        VerificationPhase::Complete => "verification_passed",
        VerificationPhase::Failed => "verification_escalated",
        VerificationPhase::Executing | VerificationPhase::Verifying => "verification_pending",
    }
}

pub(super) fn start_verification_job(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: CompletionContext,
    test_command: Option<String>,
) {
    let worktree_dir = ctx.worktree_dir.clone();
    info!(
        engineer,
        task_id = ctx.task_id,
        worktree = %worktree_dir.display(),
        "starting background completion verification"
    );
    let job = TestJob::spawn(format!("verify-{engineer}"), move |child_pgid| {
        run_automatic_verification_tracked(
            &worktree_dir,
            test_command.as_deref(),
            Some(child_pgid.as_ref()),
        )
    });
    daemon
        .verification
        .background_tests
        .verification
        .insert(engineer.to_string(), VerificationJob { ctx, job });
}

/// The passing result recorded when policy skips the verification command.
pub(super) fn skipped_verification_run(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: &mut CompletionContext,
) -> VerificationRunResult {
    let output =
        "automatic verification skipped because workflow_policy.verification.auto_run_tests=false"
            .to_string();
    ctx.verification_state.last_test_passed = true;
    ctx.verification_state.last_test_output = Some(output.clone());
    record_verification_evidence(
        daemon,
        engineer,
        ctx.task_id,
        &mut ctx.verification_state,
        EvidenceKind::TestsPassed,
        "automatic verification skipped by policy".to_string(),
    );
    VerificationRunResult {
        passed: true,
        output,
        results: empty_test_results(
            "cargo",
            Some("automatic verification skipped by policy".to_string()),
        ),
        failures: Vec::new(),
        file_paths: Vec::new(),
        recovery: None,
    }
}

/// Apply every finished background completion verification. Called from the
/// daemon poll loop; returns the first continuation error after applying all
/// finished jobs.
pub(crate) fn poll_verification_jobs(daemon: &mut TeamDaemon) -> Result<()> {
    let finished: Vec<(String, Result<VerificationRunResult>)> = daemon
        .verification
        .background_tests
        .verification
        .iter()
        .filter_map(|(engineer, job)| job.job.try_take().map(|r| (engineer.clone(), r)))
        .collect();
    let mut first_error = None;
    for (engineer, result) in finished {
        let Some(job) = daemon
            .verification
            .background_tests
            .verification
            .remove(&engineer)
        else {
            continue;
        };
        if let Err(error) = apply_verification_result(daemon, &engineer, job, result) {
            warn!(engineer = %engineer, error = %error, "completion verification handling failed");
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn apply_verification_result(
    daemon: &mut TeamDaemon,
    engineer: &str,
    job: VerificationJob,
    result: Result<VerificationRunResult>,
) -> Result<()> {
    let VerificationJob { mut ctx, job } = job;
    let task_id = ctx.task_id;
    let duration_ms = job.elapsed().as_millis() as u64;

    let current_task = daemon.active_task_id(engineer);
    if current_task != Some(task_id) {
        info!(
            engineer,
            task_id,
            current_task = ?current_task,
            duration_ms,
            "discarding stale verification result: task is no longer active for this engineer"
        );
        return Ok(());
    }
    if ctx.worktree_head.is_some() && head_commit(&ctx.worktree_dir).ok() != ctx.worktree_head {
        info!(
            engineer,
            task_id,
            duration_ms,
            "discarding stale verification result: worktree HEAD moved while tests ran; re-verifying"
        );
        if ctx.records_test_evidence {
            ctx.verification_state.iteration = ctx.verification_state.iteration.saturating_sub(1);
        }
        daemon
            .verification
            .engineers
            .insert(engineer.to_string(), ctx.verification_state);
        return handle_engineer_completion(daemon, engineer);
    }

    let verification_run = result.with_context(|| {
        format!(
            "automatic verification failed while running tests in {}",
            ctx.worktree_dir.display()
        )
    })?;
    info!(
        engineer,
        task_id,
        passed = verification_run.passed,
        duration_ms,
        "completion verification finished"
    );

    let test_duration_ms = if ctx.records_test_evidence {
        ctx.verification_state.last_test_passed = verification_run.passed;
        ctx.verification_state.last_test_output = Some(verification_run.output.clone());
        if verification_run.passed {
            record_verification_evidence(
                daemon,
                engineer,
                task_id,
                &mut ctx.verification_state,
                EvidenceKind::TestsPassed,
                "tests_passed".to_string(),
            );
        } else {
            record_verification_evidence(
                daemon,
                engineer,
                task_id,
                &mut ctx.verification_state,
                EvidenceKind::TestsFailed,
                verification_run
                    .failures
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "tests_failed".to_string()),
            );
        }
        duration_ms
    } else {
        0
    };
    finish_engineer_completion(daemon, engineer, ctx, verification_run, test_duration_ms)
}

/// The engineer verified on the wrong branch: record `branch_recovery_required`
/// without consuming a verification retry, and tell engineer and manager.
pub(super) fn handle_branch_recovery(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: CompletionContext,
    recovery: &VerificationRecovery,
    verification_run: &VerificationRunResult,
) -> Result<()> {
    let CompletionContext {
        task_id,
        board_dir,
        manager_name,
        mut verification_state,
        ..
    } = ctx;
    verification_state.iteration = verification_state.iteration.saturating_sub(1);
    verification_state.last_test_passed = false;
    verification_state.last_test_output = Some(verification_run.output.clone());
    persist_verification_recovery_snapshot(
        daemon.project_root(),
        &board_dir,
        task_id,
        engineer,
        &verification_state,
        verification_run,
    )?;

    let recovery_message = verification_recovery_message(recovery);
    daemon.record_orchestrator_action(format!(
        "verification recovery: task #{} for {} is on wrong branch '{}' (expected '{}'); recorded branch_recovery_required without consuming retry",
        recovery.expected_task_id, engineer, recovery.actual_branch, recovery.expected_branch
    ));
    daemon.queue_message("batty", engineer, &recovery_message)?;
    if let Some(ref manager_name) = manager_name {
        daemon.queue_message(
            "daemon",
            manager_name,
            &format!(
                "[daemon] Branch recovery required before verifying task #{} for {}.\n{}",
                recovery.expected_task_id, engineer, recovery_message
            ),
        )?;
    }
    daemon.verification.engineers.remove(engineer);
    Ok(())
}

/// Mark verification complete for an accepted completion and persist the
/// passing snapshot.
pub(super) fn complete_verification(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: &mut CompletionContext,
    verification_run: &VerificationRunResult,
) -> Result<()> {
    let task_id = ctx.task_id;
    daemon
        .verification
        .narration_rejection_counts
        .remove(&task_id);
    transition_verification_phase(
        daemon,
        engineer,
        task_id,
        &mut ctx.verification_state,
        VerificationPhase::Complete,
    );
    persist_verification_snapshot(
        daemon.project_root(),
        &ctx.board_dir,
        task_id,
        engineer,
        &ctx.verification_state,
        verification_run,
    )?;
    daemon.verification.engineers.remove(engineer);
    Ok(())
}

/// Record the passing test results (with failures that passed this time
/// reported as flaky) in the telemetry database.
pub(super) fn record_test_telemetry(
    daemon: &mut TeamDaemon,
    engineer: &str,
    ctx: &CompletionContext,
    verification_run: &VerificationRunResult,
) -> Result<()> {
    let previous_results = load_previous_test_results(&ctx.board_dir, ctx.task_id)?;
    let flaky_failures = if verification_run.passed {
        detect_flaky_failures(previous_results.as_ref(), &verification_run.results)
    } else {
        Vec::new()
    };
    if let Some(conn) = &daemon.telemetry_db {
        telemetry_db::record_test_results(
            conn,
            ctx.task_id,
            engineer,
            &verification_run.results,
            &flaky_failures,
        )?;
    }
    Ok(())
}

pub(super) fn persist_verification_snapshot(
    project_root: &Path,
    board_dir: &Path,
    task_id: u32,
    engineer: &str,
    state: &VerificationState,
    verification_run: &VerificationRunResult,
) -> Result<String> {
    let attempt = state.iteration.max(1);
    let relative_path = Path::new(".batty")
        .join("reports")
        .join("verification")
        .join("completion")
        .join(format!(
            "task-{task_id:03}-{engineer}-attempt-{attempt}.json"
        ));
    let absolute_path = project_root.join(&relative_path);
    if let Some(parent) = absolute_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let snapshot = serde_json::json!({
        "task_id": task_id,
        "engineer": engineer,
        "phase": format!("{:?}", state.phase).to_ascii_lowercase(),
        "attempt": attempt,
        "max_attempts": state.max_iterations,
        "outcome": verification_outcome_label(&state.phase),
        "passed": verification_run.passed,
        "results": verification_run.results,
        "failures": verification_run.failures,
        "file_paths": verification_run.file_paths,
        "evidence": state.evidence,
        "output": verification_run.output,
    });
    let serialized =
        serde_json::to_string_pretty(&snapshot).context("failed to serialize verification data")?;
    std::fs::write(&absolute_path, serialized)
        .with_context(|| format!("failed to write {}", absolute_path.display()))?;

    let task_path = crate::team::task_cmd::find_task_path(board_dir, task_id)?;
    crate::team::completion::validate_completion_metadata_target(
        project_root,
        &task_path,
        task_id,
        None,
        &[relative_path.to_string_lossy().into_owned()],
    )?;
    let mut metadata = crate::team::board::read_workflow_metadata(&task_path)?;
    metadata.tests_run = Some(true);
    metadata.tests_passed = Some(verification_run.passed);
    metadata.test_results = Some(verification_run.results.clone());
    metadata.outcome = Some(verification_outcome_label(&state.phase).to_string());
    track_artifact(&mut metadata, &relative_path.to_string_lossy());
    crate::team::board::write_workflow_metadata(&task_path, &metadata)?;

    Ok(relative_path.to_string_lossy().into_owned())
}

fn persist_verification_recovery_snapshot(
    project_root: &Path,
    board_dir: &Path,
    task_id: u32,
    engineer: &str,
    state: &VerificationState,
    verification_run: &VerificationRunResult,
) -> Result<String> {
    let relative_path = Path::new(".batty")
        .join("reports")
        .join("verification")
        .join("completion")
        .join(format!("task-{task_id:03}-{engineer}-attempt-1.json"));
    let absolute_path = project_root.join(&relative_path);
    if let Some(parent) = absolute_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let snapshot = serde_json::json!({
        "task_id": task_id,
        "engineer": engineer,
        "phase": "branch_recovery",
        "attempt": state.iteration,
        "max_attempts": state.max_iterations,
        "outcome": "branch_recovery_required",
        "passed": false,
        "results": verification_run.results,
        "failures": verification_run.failures,
        "file_paths": verification_run.file_paths,
        "evidence": state.evidence,
        "recovery": verification_run.recovery,
        "output": verification_run.output,
    });
    let serialized =
        serde_json::to_string_pretty(&snapshot).context("failed to serialize verification data")?;
    std::fs::write(&absolute_path, serialized)
        .with_context(|| format!("failed to write {}", absolute_path.display()))?;

    let task_path = crate::team::task_cmd::find_task_path(board_dir, task_id)?;
    crate::team::completion::validate_completion_metadata_target(
        project_root,
        &task_path,
        task_id,
        None,
        &[relative_path.to_string_lossy().into_owned()],
    )?;
    let mut metadata = crate::team::board::read_workflow_metadata(&task_path)?;
    metadata.tests_run = Some(true);
    metadata.tests_passed = Some(false);
    metadata.test_results = Some(verification_run.results.clone());
    metadata.outcome = Some("branch_recovery_required".to_string());
    if let Some(recovery) = verification_run.recovery.as_ref() {
        metadata.branch = Some(recovery.expected_branch.clone());
        metadata.commit = branch_head(project_root, &recovery.expected_branch);
        if let Some(worktree_path) = worktree_path_for_branch(&recovery.expected_branch) {
            metadata.worktree_path = Some(worktree_path);
        }
    }
    track_artifact(&mut metadata, &relative_path.to_string_lossy());
    crate::team::board::write_workflow_metadata(&task_path, &metadata)?;

    Ok(relative_path.to_string_lossy().into_owned())
}

fn worktree_path_for_branch(branch: &str) -> Option<String> {
    if crate::worktree::ops::is_archive_branch(branch) {
        return None;
    }
    let engineer = branch.split('/').next()?.trim();
    (!engineer.is_empty()).then(|| format!(".batty/worktrees/{engineer}"))
}

fn empty_test_results(framework: &str, summary: Option<String>) -> TestResults {
    TestResults {
        framework: framework.to_string(),
        total: None,
        passed: 0,
        failed: 0,
        ignored: 0,
        failures: Vec::new(),
        summary,
    }
}

fn load_previous_test_results(board_dir: &Path, task_id: u32) -> Result<Option<TestResults>> {
    let task_path = crate::team::task_cmd::find_task_path(board_dir, task_id)?;
    let metadata = crate::team::board::read_workflow_metadata(&task_path)?;
    Ok(metadata.test_results)
}

fn detect_flaky_failures(
    previous_results: Option<&TestResults>,
    current_results: &TestResults,
) -> Vec<TestFailure> {
    let Some(previous) = previous_results else {
        return Vec::new();
    };
    if previous.failed == 0 || current_results.failed != 0 {
        return Vec::new();
    }
    previous.failures.clone()
}

/// Test helper: poll until every background completion verification has
/// been applied (or panic after a generous timeout).
#[cfg(test)]
pub(crate) fn wait_for_verification_jobs(daemon: &mut TeamDaemon) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        poll_verification_jobs(daemon)?;
        if daemon.verification.background_tests.verification.is_empty() {
            return Ok(());
        }
        assert!(
            Instant::now() < deadline,
            "background verification did not finish in time"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
