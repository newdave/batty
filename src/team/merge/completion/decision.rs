//! Merge routing for a verified completion: confidence scoring, the
//! auto-merge decision, and the manual-review routes.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::team::auto_merge::{self, AutoMergeDecisionKind, AutoMergeDecisionRecord};
use crate::team::board::{read_workflow_metadata, write_workflow_metadata};
use crate::team::daemon::TeamDaemon;
use crate::team::task_loop::{current_worktree_branch, read_task_title};
use crate::team::test_results::TestResults;

use super::super::git_ops::{commits_ahead_of_trunk, head_commit};
use super::multi_repo::multi_repo_commits_ahead_of_trunk;
use super::{VerifiedCompletion, task_status};

/// Diff size and confidence carried on a merge-queue request.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct MergeStats {
    pub confidence: f64,
    pub files_changed: usize,
    pub lines_changed: usize,
}

impl MergeStats {
    fn from_decision(decision: &AutoMergeDecisionRecord) -> Self {
        Self {
            confidence: decision.confidence,
            files_changed: decision.files_changed,
            lines_changed: decision.lines_changed,
        }
    }
}

/// Where a verified completion goes next.
#[derive(Debug)]
pub(super) enum MergeRoute {
    /// A per-task override disabled auto-merge.
    OverrideManualReview,
    /// Auto-merge policy sent the task to manual review.
    ManualReview(AutoMergeDecisionRecord),
    /// Auto-merge policy accepted the task.
    AutoMerge(MergeStats),
    /// Auto-merge is off: merge without a review gate.
    Direct(MergeStats),
}

/// Score the diff (always, for observability) and decide the route. Every
/// auto-merge decision is recorded as an event.
pub(super) fn decide_merge_route(
    daemon: &mut TeamDaemon,
    verified: &VerifiedCompletion<'_>,
) -> MergeRoute {
    let engineer = verified.engineer;
    let task_id = verified.ctx.task_id;
    let policy = daemon.config.team_config.workflow_policy.auto_merge.clone();
    let auto_merge_override = daemon
        .merge
        .auto_merge_override(&daemon.config.project_root, task_id);

    let diff_analysis = analyze_completion_diff(
        daemon.project_root(),
        daemon.is_multi_repo,
        &daemon.sub_repo_names,
        &verified.ctx.worktree_dir,
        &verified.ctx.trunk_branch,
        verified.task_branch,
    );
    let mut scored = MergeStats::default();
    if let Ok(ref summary) = diff_analysis {
        let confidence = auto_merge::score_auto_merge_candidate(summary, &policy);
        scored = MergeStats {
            confidence,
            files_changed: summary.files_changed,
            lines_changed: summary.total_lines(),
        };
        let task_str = task_id.to_string();
        let info = crate::team::events::MergeConfidenceInfo {
            engineer,
            task: &task_str,
            confidence,
            files_changed: summary.files_changed,
            lines_changed: summary.total_lines(),
            has_migrations: summary.has_migrations,
            has_config_changes: summary.has_config_changes,
            rename_count: summary.rename_count,
        };
        daemon.record_merge_confidence_scored(&info);
    }

    if auto_merge_override == Some(false) {
        let decision =
            auto_merge::forced_manual_review_decision(diff_analysis.as_ref().ok(), &policy, true);
        daemon.record_auto_merge_decision(engineer, task_id, &decision);
        return MergeRoute::OverrideManualReview;
    }

    if auto_merge_override != Some(true) && !policy.enabled {
        return MergeRoute::Direct(scored);
    }

    // Fail closed: a diff we could not analyze is never auto-merged.
    let decision = match diff_analysis {
        Ok(ref summary) if auto_merge_override == Some(true) => {
            auto_merge::forced_auto_merge_decision(Some(summary), &policy, true)
        }
        Ok(ref summary) => auto_merge::evaluate_auto_merge_candidate(summary, &policy, true),
        Err(ref error) => {
            warn!(engineer, task_id, error = %format!("{error:#}"), "auto-merge diff analysis failed, routing to manual review");
            auto_merge::analysis_failed_decision(&format!("{error:#}"), true)
        }
    };
    daemon.record_auto_merge_decision(engineer, task_id, &decision);
    match decision.decision {
        AutoMergeDecisionKind::Accepted => {
            info!(
                engineer,
                task_id,
                confidence = decision.confidence,
                files = decision.files_changed,
                lines = decision.lines_changed,
                "auto-merging task"
            );
            MergeRoute::AutoMerge(MergeStats::from_decision(&decision))
        }
        AutoMergeDecisionKind::ManualReview => MergeRoute::ManualReview(decision),
    }
}

/// Per-task override disabled auto-merge: move to review and tell the
/// manager. No completion packet is written for this route.
pub(super) fn route_override_to_review(
    daemon: &mut TeamDaemon,
    verified: &VerifiedCompletion<'_>,
) -> Result<()> {
    let engineer = verified.engineer;
    let ctx = verified.ctx;
    let task_id = ctx.task_id;
    info!(
        engineer,
        task_id, "auto-merge disabled by per-task override, routing to manual review"
    );
    if !move_task_to_review(
        daemon,
        &ctx.board_dir,
        task_id,
        ctx.manager_name.as_deref(),
        engineer,
    )? {
        return Ok(());
    }
    if let Some(ref manager_name) = ctx.manager_name {
        let task_title = read_task_title(&ctx.board_dir, task_id);
        let msg = format!(
            "[{engineer}] Task #{task_id} passed tests. Auto-merge disabled by override — awaiting manual review.\nTitle: {task_title}"
        );
        daemon.queue_message(engineer, manager_name, &msg)?;
        daemon.mark_member_working(manager_name);
    }
    daemon.clear_active_task(engineer);
    daemon.record_task_completed(engineer, Some(task_id));
    daemon.set_member_idle(engineer);
    Ok(())
}

/// Auto-merge policy wants a human: move to review with a completion packet
/// and tell the manager why.
pub(super) fn route_to_manual_review(
    daemon: &mut TeamDaemon,
    verified: &VerifiedCompletion<'_>,
    decision: &AutoMergeDecisionRecord,
) -> Result<()> {
    let engineer = verified.engineer;
    let ctx = verified.ctx;
    let task_id = ctx.task_id;
    info!(
        engineer,
        task_id,
        confidence = decision.confidence,
        reasons = ?decision.reasons,
        "routing to manual review"
    );
    if !move_to_review_with_packet(daemon, verified)? {
        return Ok(());
    }
    if let Some(ref manager_name) = ctx.manager_name {
        let task_title = read_task_title(&ctx.board_dir, task_id);
        let reason_text = decision.reasons.join("; ");
        let msg = format!(
            "[{engineer}] Task #{task_id} passed tests but requires manual review.\nTitle: {task_title}\nConfidence: {confidence:.2}\nReasons: {reason_text}",
            confidence = decision.confidence
        );
        daemon.queue_message(engineer, manager_name, &msg)?;
        daemon.mark_member_working(manager_name);
    }
    daemon.clear_active_task(engineer);
    daemon.record_task_completed(engineer, Some(task_id));
    daemon.set_member_idle(engineer);
    Ok(())
}

/// Move the task to review and write its completion packet. Returns false
/// when the task was not moved (no commits) or the packet was rejected and
/// escalated; the caller then stops.
pub(super) fn move_to_review_with_packet(
    daemon: &mut TeamDaemon,
    verified: &VerifiedCompletion<'_>,
) -> Result<bool> {
    let engineer = verified.engineer;
    let ctx = verified.ctx;
    let task_id = ctx.task_id;
    if !move_task_to_review(
        daemon,
        &ctx.board_dir,
        task_id,
        ctx.manager_name.as_deref(),
        engineer,
    )? {
        return Ok(false);
    }
    if let Err(error) = record_completion_packet_metadata(
        daemon.project_root(),
        &ctx.board_dir,
        task_id,
        verified.task_branch,
        &ctx.worktree_dir,
        true,
        Some(verified.verification_run.results.clone()),
    ) {
        escalate_completion_metadata_rejection(
            daemon,
            engineer,
            task_id,
            ctx.manager_name.as_deref(),
            &error,
        )?;
        return Ok(false);
    }
    Ok(true)
}

/// Record the completion packet metadata on a task file after the daemon has
/// successfully verified an engineer's work. This mirrors what
/// `completion::ingest_completion_message` does for messages containing a
/// literal `Completion Packet` block, and is required by
/// `merge_queue::missing_completion_packet_detail` before the merge queue
/// will process the request. Without this step, every daemon-initiated
/// auto-merge is skipped with `missing_packet` — a silent throughput killer
/// observed under multi-engineer load.
pub(super) fn record_completion_packet_metadata(
    project_root: &Path,
    board_dir: &Path,
    task_id: u32,
    branch: &str,
    worktree_dir: &Path,
    tests_passed: bool,
    test_results: Option<TestResults>,
) -> Result<()> {
    let task_path = crate::team::task_cmd::find_task_path(board_dir, task_id)
        .with_context(|| format!("failed to locate task #{task_id} for completion metadata"))?;
    crate::team::completion::validate_completion_metadata_target(
        project_root,
        &task_path,
        task_id,
        Some(branch),
        &[],
    )?;
    let mut metadata = read_workflow_metadata(&task_path)?;
    metadata.branch = Some(branch.to_string());
    metadata.commit = Some(head_commit(worktree_dir)?);
    metadata.worktree_path = Some(relativize_worktree_path(project_root, worktree_dir));
    metadata.tests_run = Some(true);
    metadata.tests_passed = Some(tests_passed);
    if metadata
        .outcome
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        metadata.outcome = Some("ready_for_review".to_string());
    }
    if test_results.is_some() {
        metadata.test_results = test_results;
    }
    write_workflow_metadata(&task_path, &metadata).with_context(|| {
        format!("failed to persist completion packet metadata for task #{task_id}")
    })
}

pub(super) fn escalate_completion_metadata_rejection(
    daemon: &mut TeamDaemon,
    engineer: &str,
    task_id: u32,
    manager_name: Option<&str>,
    error: &anyhow::Error,
) -> Result<()> {
    warn!(
        engineer,
        task_id,
        error = %error,
        "completion metadata rejected; escalating without writing review metadata"
    );
    daemon.record_task_escalated(
        engineer,
        task_id.to_string(),
        Some("completion_metadata_mismatch"),
    );
    let message = format!(
        "[daemon] Completion metadata rejected for {engineer} on task #{task_id}; Batty did not write branch/commit/review metadata because the task id, branch, or artifacts did not agree.\nReason: {error:#}"
    );
    if let Some(manager_name) = manager_name {
        daemon.queue_message("daemon", manager_name, &message)?;
        daemon.mark_member_working(manager_name);
    } else {
        daemon.record_orchestrator_action(message);
    }
    daemon.clear_active_task(engineer);
    daemon.set_member_idle(engineer);
    Ok(())
}

fn relativize_worktree_path(project_root: &Path, worktree_dir: &Path) -> String {
    match worktree_dir.strip_prefix(project_root) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => worktree_dir.to_string_lossy().into_owned(),
    }
}

/// Move the task to review (walking the legal path from an unexpected
/// state) and set the review owner. Refuses — returning false and telling
/// the engineer — when the branch has no commits ahead of trunk.
pub(super) fn move_task_to_review(
    daemon: &mut TeamDaemon,
    board_dir: &Path,
    task_id: u32,
    manager_name: Option<&str>,
    engineer: &str,
) -> Result<bool> {
    let worktree_dir = daemon.worktree_dir(engineer);
    let trunk_branch = daemon.config.team_config.trunk_branch().to_string();
    let commits_ahead = if daemon.is_multi_repo {
        multi_repo_commits_ahead_of_trunk(&worktree_dir, &daemon.sub_repo_names, &trunk_branch)?
    } else {
        commits_ahead_of_trunk(&worktree_dir, &trunk_branch)?
    };

    if commits_ahead == 0 {
        warn!(
            engineer,
            task_id,
            trunk_branch,
            "refusing to move task to review because branch has no commits ahead of trunk"
        );
        let msg = format!(
            "Review blocked: your branch has no commits ahead of {trunk_branch}, so Batty kept the task in progress instead of moving it to review.\n\
                   Commit the work first:\n\
                   ```\n\
                   git add -A\n\
                   git commit -m \"your task description\"\n\
                   ```"
        );
        daemon.queue_message("batty", engineer, &msg)?;
        return Ok(false);
    }

    if let Err(error) = crate::team::task_cmd::transition_task_with_attribution(
        board_dir,
        task_id,
        "review",
        crate::team::task_cmd::StatusTransitionAttribution::daemon("daemon.merge.completion"),
    ) {
        warn!(
            engineer,
            task_id,
            error = %error,
            "failed to move task to review — attempting force via in-progress first"
        );
        // If the task is in an unexpected state (e.g. blocked or backlog), walk
        // the legal path (Backlog -> Todo -> InProgress -> Review) so completion
        // does not fail on the same transition forever.
        let current = task_status(board_dir, task_id);
        let steps: &[&str] = match current.as_deref() {
            Some("backlog") => &["todo", "in-progress", "review"],
            _ => &["in-progress", "review"],
        };
        let mut last_error = None;
        for step in steps {
            if let Err(step_error) = crate::team::task_cmd::transition_task_with_attribution(
                board_dir,
                task_id,
                step,
                crate::team::task_cmd::StatusTransitionAttribution::daemon(
                    "daemon.merge.completion",
                ),
            ) {
                last_error = Some(step_error);
                break;
            }
        }
        if let Some(error2) = last_error {
            warn!(
                engineer,
                task_id,
                error = %error2,
                "force review transition also failed — leaving task in current state"
            );
        }
    }
    if let Some(manager_name) = manager_name
        && let Err(error) = crate::team::task_cmd::assign_task_owners(
            board_dir,
            task_id,
            Some(engineer),
            Some(manager_name),
        )
    {
        warn!(
            engineer,
            task_id,
            manager = manager_name,
            error = %error,
            "failed to set review owner"
        );
    }
    Ok(true)
}

/// Analyze a completed task's diff in the repo(s) that actually hold the
/// branch. In multi-repo mode the project root is not a git repo, so each
/// sub-repo with commits ahead is analyzed and the results combined.
pub(super) fn analyze_completion_diff(
    project_root: &Path,
    is_multi_repo: bool,
    sub_repo_names: &[String],
    worktree_dir: &Path,
    trunk_branch: &str,
    task_branch: &str,
) -> Result<auto_merge::DiffSummary> {
    if !is_multi_repo {
        return auto_merge::analyze_diff(project_root, trunk_branch, task_branch);
    }
    let mut repos = Vec::new();
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if !sub_wt.exists() {
            continue;
        }
        if commits_ahead_of_trunk(&sub_wt, trunk_branch)? == 0 {
            continue;
        }
        repos.push((project_root.join(name), current_worktree_branch(&sub_wt)?));
    }
    auto_merge::analyze_diff_across_repos(&repos, trunk_branch)
}
