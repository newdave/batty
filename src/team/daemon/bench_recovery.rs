//! Bench recovery: hand parked work on, reset it onto another engineer at a
//! higher effort, or block it once the reset cap is reached.
//!
//! The persisted per-task state (reset count, avoid list, effort override)
//! lives in task frontmatter via `team::bench_recovery`. Bench-file access
//! always goes through the short-lived bench lock; git, messaging and board
//! writes here never run inside it.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use tracing::{debug, info, warn};

use super::TeamDaemon;
use super::background_tests::{BenchApprovalJob, BenchApprovalKind};
use super::completion_checks::{VerificationRunResult, run_automatic_verification_tracked};
use super::merge_queue::MergeOrigin;
use crate::team::bench::BenchEntry;
use crate::team::bench_recovery::{
    BarEvidence, BenchRecoveryFrontmatter, attributed_commit_count, bar_commits_met,
    effort_ladder_for_backend, next_effort, read_bench_recovery, verification_meets_bar,
    write_bench_recovery,
};
use crate::team::config::RoleType;
use crate::team::events::TeamEvent;
use crate::team::task_cmd::{self, StatusTransitionAttribution};
use crate::team::test_job::TestJob;

/// `blocked_on` reason for a task that hit `max_auto_resets`.
pub(super) const RESET_CAP_REASON: &str = "bench recovery: reset cap reached";
const RESET_NEXT_ACTION: &str = "reset by bench recovery";

impl TeamDaemon {
    /// Names of the human (`RoleType::User`) roles.
    pub(super) fn human_role_names(&self) -> Vec<String> {
        self.config
            .team_config
            .roles
            .iter()
            .filter(|role| role.role_type == RoleType::User)
            .map(|role| role.name.clone())
            .collect()
    }

    /// Reset a parked task so another engineer can pick it up, or block it
    /// when it has already been reset `max_auto_resets` times. Either way the
    /// engineer is released and unbenched.
    pub(super) fn reset_parked_task(&mut self, engineer: &str, task_id: u32) -> Result<()> {
        let policy = self
            .config
            .team_config
            .workflow_policy
            .bench_recovery
            .clone();
        let board_dir = self.board_dir();
        let task_path = task_cmd::find_task_path(&board_dir, task_id)?;
        let task = crate::task::Task::from_file(&task_path)?;
        let mut state = read_bench_recovery(&task_path);
        if state.bench_reset_count >= policy.max_auto_resets {
            return self.cap_parked_task(engineer, task_id, &task_path, state);
        }

        // Keep the old attempt: auto-save dirty work onto the task branch,
        // then archive the branch under preserved/* so the next dispatch can
        // start a fresh branch from trunk without losing anything.
        let task_branch = task
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::worktree::ops::engineer_task_branch_name(engineer, task_id));
        let mut archived = None;
        if self.is_git_repo {
            let worktree = self.worktree_dir(engineer);
            if worktree.is_dir() {
                self.preserve_worktree_before_restart(engineer, &worktree, "bench_recovery_reset");
            }
            archived = crate::worktree::ops::archive_branch_if_commits_ahead(
                self.project_root(),
                &task_branch,
                self.config.team_config.trunk_branch(),
                "bench_recovery",
            )
            .with_context(|| {
                format!("failed to archive {task_branch} before resetting task #{task_id}")
            })?;
        }

        self.clear_active_task(engineer);
        self.verification.clear_zero_diff_completion(task_id);
        task_cmd::reclaim_task_claim_with_attribution(
            &board_dir,
            task_id,
            RESET_NEXT_ACTION,
            StatusTransitionAttribution::daemon("daemon.bench_recovery.reset"),
        )?;
        // reclaim leaves the pinned assignee (a hard dispatch filter) and the
        // assignment context in place; drop them so anyone can take the task.
        task_cmd::update_task_frontmatter(&task_path, |mapping| {
            task_cmd::set_optional_string(mapping, "assignee", None);
            task_cmd::set_optional_string(mapping, "branch", None);
            task_cmd::set_optional_string(mapping, "worktree_path", None);
        })?;

        if !state.avoid_engineers.iter().any(|name| name == engineer) {
            state.avoid_engineers.push(engineer.to_string());
        }
        let (backend, configured_effort) = self.member_backend_and_effort(engineer);
        let ladder = effort_ladder_for_backend(&policy.effort_ladder, backend.as_deref());
        let current = state.effort_override.clone().or(configured_effort);
        match next_effort(ladder, current.as_deref()) {
            Some(effort) => state.effort_override = Some(effort),
            None => debug!(
                engineer,
                task_id,
                backend = ?backend,
                "bench recovery: no effort ladder step for this backend; effort unchanged"
            ),
        }
        state.bench_reset_count += 1;
        write_bench_recovery(&task_path, &state)?;
        self.unbench_after_recovery(engineer);

        let effort = state.effort_override.as_deref().unwrap_or("unchanged");
        let reason = format!(
            "reset {}/{}, effort {effort}",
            state.bench_reset_count, policy.max_auto_resets
        );
        self.emit_event(TeamEvent::task_bench_reset(
            engineer,
            &task_id.to_string(),
            &reason,
        ));
        let _ = self.queue_daemon_message(
            engineer,
            &format!(
                "Task #{task_id} was reset by bench recovery and goes back to the queue for another engineer. Stop work on it; you are no longer benched."
            ),
        );
        let archive_note = archived
            .map(|branch| format!(", old branch kept as {branch}"))
            .unwrap_or_default();
        self.record_orchestrator_action(format!(
            "recovery: reset task #{task_id} from {engineer} back to todo ({reason}{archive_note})"
        ));
        Ok(())
    }

    /// The reset cap is reached: block the task, tell the humans, release
    /// and unbench the engineer.
    fn cap_parked_task(
        &mut self,
        engineer: &str,
        task_id: u32,
        task_path: &Path,
        mut state: BenchRecoveryFrontmatter,
    ) -> Result<()> {
        let board_dir = self.board_dir();
        task_cmd::block_task_with_reason_and_attribution(
            &board_dir,
            task_id,
            RESET_CAP_REASON,
            StatusTransitionAttribution::daemon("daemon.bench_recovery.cap"),
        )?;
        task_cmd::release_engineer_claim(&board_dir, task_id)?;
        state.bench_reset_capped = true;
        write_bench_recovery(task_path, &state)?;

        self.clear_active_task(engineer);
        self.verification.clear_zero_diff_completion(task_id);
        self.record_task_escalated(engineer, task_id.to_string(), Some("bench_reset_capped"));
        let body = format!(
            "Task #{task_id} is blocked: bench recovery reset it {} time(s) and it still has no attributed, verified work (last engineer: {engineer}). It needs a human decision.",
            state.bench_reset_count
        );
        for human in self.human_role_names() {
            let _ = self.queue_daemon_message(&human, &body);
        }
        self.emit_event(TeamEvent::task_bench_reset_capped(
            engineer,
            &task_id.to_string(),
            RESET_CAP_REASON,
        ));
        self.unbench_after_recovery(engineer);
        self.record_orchestrator_action(format!(
            "recovery: blocked task #{task_id} after {} bench recovery reset(s); escalated to the human",
            state.bench_reset_count
        ));
        Ok(())
    }

    /// Remove the engineer from the bench. Failures only log: a later bench
    /// recovery pass unbenches entries whose task is no longer active.
    pub(super) fn unbench_after_recovery(&self, engineer: &str) {
        if let Err(error) = crate::team::bench::unbench_engineer(self.project_root(), engineer) {
            warn!(engineer, error = %error, "bench recovery could not unbench engineer");
        }
    }

    /// The member's backend and configured effort (before any task override).
    fn member_backend_and_effort(&self, member: &str) -> (Option<String>, Option<String>) {
        let options = crate::team::agent_options::all(&self.config.team_config)
            .ok()
            .and_then(|all| all.into_iter().find(|options| options.member == member));
        match options {
            Some(options) => (options.backend, options.effort),
            None => (
                self.config
                    .members
                    .iter()
                    .find(|m| m.name == member)
                    .and_then(|m| m.agent.clone()),
                None,
            ),
        }
    }
}

impl TeamDaemon {
    /// True when every dispatchable engineer (one whose role is not
    /// `on_demand`) has a bench entry. No dispatchable engineers is never
    /// "all benched".
    pub(super) fn all_engineers_benched(&self, entries: &BTreeMap<String, BenchEntry>) -> bool {
        let mut dispatchable = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type == RoleType::Engineer)
            .filter(|member| {
                !self
                    .config
                    .team_config
                    .role_def(&member.role_name)
                    .is_some_and(|role| role.on_demand)
            })
            .peekable();
        dispatchable.peek().is_some()
            && dispatchable.all(|member| entries.contains_key(&member.name))
    }

    /// [`Self::all_engineers_benched`] against a fresh read of the bench file
    /// (`batty unbench` can change it at any time). Unreadable state counts
    /// as not all benched.
    fn all_engineers_benched_now(&self) -> bool {
        match crate::team::bench::bench_entries(self.project_root()) {
            Ok(entries) => self.all_engineers_benched(&entries),
            Err(error) => {
                warn!(error = %error, "bench recovery could not read the bench file");
                false
            }
        }
    }

    /// Start verifying a review task for approval while every engineer is
    /// benched. Returns false when nothing was started (auto-merge disabled,
    /// already queued or already being verified).
    pub fn start_bench_review_approval(&mut self, task_id: u32) -> Result<bool> {
        let task = self.bench_task(task_id)?;
        let engineer = task
            .claimed_by
            .clone()
            .ok_or_else(|| anyhow!("Task #{task_id} is not assigned."))?;
        self.start_bench_approval_job(task_id, &engineer, BenchApprovalKind::ApproveReview)
    }

    /// Start verifying a parked engineer's task: it is handed on to merge if
    /// it meets the bar and reset otherwise.
    pub fn start_parked_task_recovery(&mut self, engineer: &str, task_id: u32) -> Result<bool> {
        self.start_bench_approval_job(task_id, engineer, BenchApprovalKind::RecoverParked)
    }

    fn bench_task(&self, task_id: u32) -> Result<crate::task::Task> {
        let task_path = task_cmd::find_task_path(&self.board_dir(), task_id)?;
        crate::task::Task::from_file(&task_path)
    }

    /// Mirror of the chat `merge` command: validate now, then run the
    /// verification command on the task branch in a background job. The
    /// claim is still held, so the verification's claim and branch checks
    /// see the task as it was parked or reviewed.
    fn start_bench_approval_job(
        &mut self,
        task_id: u32,
        engineer: &str,
        kind: BenchApprovalKind,
    ) -> Result<bool> {
        let task = self.bench_task(task_id)?;
        match kind {
            BenchApprovalKind::ApproveReview => {
                if task.status != "review" {
                    bail!("Task #{task_id} is not in review.");
                }
                if !self.config.team_config.workflow_policy.auto_merge.enabled {
                    return Ok(false);
                }
            }
            BenchApprovalKind::RecoverParked => {
                if !matches!(task.status.as_str(), "in-progress" | "review") {
                    bail!("Task #{task_id} is not in progress or in review.");
                }
            }
        }
        if task.claimed_by.as_deref() != Some(engineer) {
            bail!("Task #{task_id} is not claimed by {engineer}.");
        }
        if self.is_multi_repo {
            bail!("Bench recovery approval is not supported for multi-repo projects.");
        }
        if self.merge.queue.contains_task(task_id)
            || self
                .verification
                .background_tests
                .bench_approvals
                .contains_key(&task_id)
        {
            return Ok(false);
        }

        let worktree_dir = self.worktree_dir(engineer);
        if !worktree_dir.is_dir() {
            bail!(
                "Task #{task_id}: worktree {} for {engineer} is missing.",
                worktree_dir.display()
            );
        }
        let policy = &self.config.team_config.workflow_policy;
        let test_command = policy
            .verification
            .test_command
            .clone()
            .or(policy.test_command.clone());
        let job = TestJob::spawn_with_timeout(
            format!("bench-approval-{task_id}"),
            self.test_job_timeout(),
            move |child_pgid| {
                run_automatic_verification_tracked(
                    &worktree_dir,
                    test_command.as_deref(),
                    Some(child_pgid.as_ref()),
                )
            },
        );
        self.verification.background_tests.bench_approvals.insert(
            task_id,
            BenchApprovalJob {
                task_id,
                engineer: engineer.to_string(),
                kind,
                job,
            },
        );
        self.record_orchestrator_action(format!(
            "recovery: verifying task #{task_id} from {engineer} for bench recovery ({})",
            kind_label(kind)
        ));
        Ok(true)
    }

    /// Finish bench recovery verifications whose test command has completed.
    pub(super) fn poll_bench_approval_jobs(&mut self) {
        let finished: Vec<(u32, Result<VerificationRunResult>)> = self
            .verification
            .background_tests
            .bench_approvals
            .iter()
            .filter_map(|(task_id, job)| job.job.try_take().map(|result| (*task_id, result)))
            .collect();
        for (task_id, result) in finished {
            let Some(job) = self
                .verification
                .background_tests
                .bench_approvals
                .remove(&task_id)
            else {
                continue;
            };
            let duration_ms = job.job.elapsed().as_millis() as u64;
            info!(
                task_id,
                engineer = %job.engineer,
                kind = kind_label(job.kind),
                duration_ms,
                "bench recovery verification finished"
            );
            if let Err(error) = self.finish_bench_approval(&job, result, duration_ms) {
                warn!(task_id, engineer = %job.engineer, error = %error, "bench recovery step failed");
                self.record_orchestrator_action(format!(
                    "recovery: bench recovery for task #{task_id} from {} failed: {error}",
                    job.engineer
                ));
            }
        }
    }

    /// Mirror of `finish_chat_merge`: re-check that the task did not change
    /// while the tests ran, then approve, hand on or reset it.
    fn finish_bench_approval(
        &mut self,
        job: &BenchApprovalJob,
        result: Result<VerificationRunResult>,
        test_duration_ms: u64,
    ) -> Result<()> {
        let task_id = job.task_id;
        let engineer = job.engineer.as_str();
        let task = self.bench_task(task_id)?;
        let status_current = match job.kind {
            BenchApprovalKind::ApproveReview => task.status == "review",
            BenchApprovalKind::RecoverParked => {
                matches!(task.status.as_str(), "in-progress" | "review")
            }
        };
        if !status_current || task.claimed_by.as_deref() != Some(engineer) {
            info!(
                task_id,
                engineer,
                status = %task.status,
                "discarding stale bench recovery verification: task changed while tests ran"
            );
            return Ok(());
        }
        // A review task is only approved or reset while the whole team is
        // still benched; an unbench mid-job hands it back to people.
        if job.kind == BenchApprovalKind::ApproveReview && !self.all_engineers_benched_now() {
            info!(
                task_id,
                engineer, "bench recovery approval skipped: an engineer was unbenched"
            );
            return Ok(());
        }

        let auto_run_tests = self
            .config
            .team_config
            .workflow_policy
            .verification
            .auto_run_tests;
        let run = match result {
            Ok(run) if verification_meets_bar(&run, auto_run_tests) => Some(run),
            Ok(_) => None,
            Err(error) => {
                info!(task_id, engineer, error = %error, "bench recovery verification did not complete");
                None
            }
        };
        let task_branch = task
            .branch
            .as_deref()
            .map(str::trim)
            .filter(|branch| !branch.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| crate::worktree::ops::engineer_task_branch_name(engineer, task_id));
        let run = match run {
            Some(run) if bar_commits_met(&self.bench_bar_evidence(engineer, &task_branch)?) => run,
            _ => {
                self.record_orchestrator_action(format!(
                    "recovery: task #{task_id} from {engineer} does not meet the bench recovery bar (attributed commits ahead of trunk and a passing test run)"
                ));
                return self.reset_parked_task(engineer, task_id);
            }
        };

        let worktree_branch =
            crate::team::task_loop::current_worktree_branch(&self.worktree_dir(engineer))?;
        if worktree_branch != task_branch {
            info!(
                task_id,
                engineer,
                worktree_branch = %worktree_branch,
                "bench recovery merge skipped: worktree left the task branch"
            );
            return Ok(());
        }
        if self.merge.queue.contains_task(task_id) {
            return Ok(());
        }

        match job.kind {
            BenchApprovalKind::ApproveReview => {
                let policy = &self.config.team_config.workflow_policy;
                if !policy.auto_merge.enabled
                    || !policy.bench_recovery.auto_approve_when_all_benched
                {
                    return Ok(());
                }
                crate::team::merge::enqueue_reviewed_merge(
                    self,
                    task_id,
                    engineer,
                    run.results,
                    test_duration_ms,
                    MergeOrigin::BenchRecovery,
                )?;
                self.emit_event(TeamEvent::task_auto_approved(
                    engineer,
                    &task_id.to_string(),
                    "all engineers benched",
                ));
                self.record_orchestrator_action(format!(
                    "recovery: approved review task #{task_id} from {engineer} for merge (all engineers benched)"
                ));
            }
            BenchApprovalKind::RecoverParked => {
                if task.status == "in-progress" {
                    task_cmd::transition_task_with_attribution(
                        &self.board_dir(),
                        task_id,
                        "review",
                        StatusTransitionAttribution::daemon("daemon.bench_recovery"),
                    )?;
                }
                crate::team::merge::enqueue_reviewed_merge(
                    self,
                    task_id,
                    engineer,
                    run.results,
                    test_duration_ms,
                    MergeOrigin::BenchRecovery,
                )?;
                self.unbench_after_recovery(engineer);
                self.emit_event(TeamEvent::task_auto_approved(
                    engineer,
                    &task_id.to_string(),
                    "parked work met the bench recovery bar",
                ));
                self.record_orchestrator_action(format!(
                    "recovery: parked task #{task_id} from {engineer} meets the bar; queued for merge and unbenched {engineer}"
                ));
            }
        }
        Ok(())
    }

    /// Commit evidence for the bar: commits on the task branch ahead of
    /// trunk, and engineer commits (not `[batty]` auto-saves) ahead of the
    /// engineer's base branch.
    fn bench_bar_evidence(&self, engineer: &str, task_branch: &str) -> Result<BarEvidence> {
        let root = self.project_root();
        let commits_ahead_of_trunk = crate::worktree::ops::ahead_count(
            root,
            self.config.team_config.trunk_branch(),
            task_branch,
        )?;
        let attributed_commits = attributed_commit_count(
            root,
            &crate::worktree::ops::engineer_base_branch_name(engineer),
            task_branch,
        )?;
        Ok(BarEvidence {
            commits_ahead_of_trunk,
            attributed_commits,
        })
    }
}

fn kind_label(kind: BenchApprovalKind) -> &'static str {
    match kind {
        BenchApprovalKind::ApproveReview => "review approval",
        BenchApprovalKind::RecoverParked => "parked task",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::team::bench::{BenchSource, bench_engineer_with, bench_entries};
    use crate::team::comms::Channel;
    use crate::team::config::{BenchRecoveryConfig, RoleDef, WorkflowMode, WorkflowPolicy};
    use crate::team::errors::DeliveryError;
    use crate::team::test_support::{
        TestDaemonBuilder, engineer_member, git_ok, git_stdout, inferred_role_defs, init_git_repo,
        manager_member,
    };

    pub(super) struct RecordingChannel {
        pub(super) messages: Arc<Mutex<Vec<String>>>,
    }

    impl Channel for RecordingChannel {
        fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
            self.messages.lock().unwrap().push(message.to_string());
            Ok(())
        }

        fn channel_type(&self) -> &str {
            "test"
        }
    }

    pub(super) fn enabled_policy() -> WorkflowPolicy {
        WorkflowPolicy {
            zero_diff_recovery: crate::team::config::ZeroDiffRecovery::Park,
            bench_recovery: BenchRecoveryConfig {
                enabled: true,
                ..BenchRecoveryConfig::default()
            },
            ..WorkflowPolicy::default()
        }
    }

    pub(super) fn write_team_yaml(repo: &Path) {
        let team_dir = repo.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_dir).unwrap();
        std::fs::write(
            team_dir.join("team.yaml"),
            "name: test\nagent: codex\nroles:\n  - name: eng\n    role_type: engineer\n    instances: 2\n",
        )
        .unwrap();
    }

    fn write_parked_task(repo: &Path, task_id: u32, status: &str, engineer: &str) -> PathBuf {
        let tasks_dir = repo.join(".batty/team_config/board/tasks");
        std::fs::create_dir_all(&tasks_dir).unwrap();
        let path = tasks_dir.join(format!("{task_id:03}-parked.md"));
        std::fs::write(
            &path,
            format!(
                "---\nid: {task_id}\ntitle: parked\nstatus: {status}\npriority: high\nclaimed_by: {engineer}\nassignee: {engineer}\nbranch: {engineer}/{task_id}\nworktree_path: .batty/worktrees/{engineer}\nclass: standard\n---\n\nTask description.\n"
            ),
        )
        .unwrap();
        path
    }

    /// eng-1 parked (auto_park) on task 7 with one commit on `eng-1/7`.
    fn parked_daemon(tmp: &tempfile::TempDir) -> (PathBuf, PathBuf, TeamDaemon) {
        let repo = init_git_repo(tmp, "batty-bench-reset");
        write_team_yaml(&repo);
        let task_path = write_parked_task(&repo, 7, "in-progress", "eng-1");
        let worktree = repo.join(".batty").join("worktrees").join("eng-1");
        crate::team::task_loop::prepare_engineer_assignment_worktree(
            &repo,
            &worktree,
            "eng-1",
            "eng-1/7",
            &repo.join(".batty").join("team_config"),
        )
        .unwrap();
        std::fs::write(worktree.join("attempt.txt"), "first attempt\n").unwrap();
        git_ok(&worktree, &["add", "attempt.txt"]);
        git_ok(&worktree, &["commit", "-m", "first attempt"]);
        bench_engineer_with(
            &repo,
            "eng-1",
            Some("completion attribution requires operator review"),
            BenchSource::AutoPark,
            Some(7),
        )
        .unwrap();

        let members = vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
            engineer_member("eng-2", Some("manager"), true),
        ];
        let mut roles = inferred_role_defs(&members);
        roles.push(RoleDef::test("human".to_string(), RoleType::User));
        let mut daemon = TestDaemonBuilder::new(&repo)
            .members(members)
            .roles(roles)
            .workflow_policy(enabled_policy())
            .build();
        daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
        daemon.set_active_task_for_test("eng-1", 7);
        (repo, task_path, daemon)
    }

    pub(super) fn event_names(repo: &Path) -> Vec<String> {
        crate::team::events::read_events(&repo.join(".batty/team_config/events.jsonl"))
            .unwrap()
            .into_iter()
            .map(|event| event.event)
            .collect()
    }

    fn orchestrator_lines(repo: &Path, needle: &str) -> usize {
        std::fs::read_to_string(repo.join(".batty").join("orchestrator.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains(needle))
            .count()
    }

    #[test]
    fn reset_returns_task_to_todo_and_raises_effort() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, task_path, mut daemon) = parked_daemon(&tmp);
        daemon.verification.note_zero_diff_completion(7);

        daemon.reset_parked_task("eng-1", 7).unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "todo");
        assert!(task.claimed_by.is_none());
        assert!(task.assignee.is_none());
        assert!(task.branch.is_none());
        assert!(task.worktree_path.is_none());
        let state = read_bench_recovery(&task_path);
        assert_eq!(state.bench_reset_count, 1);
        assert_eq!(state.avoid_engineers, vec!["eng-1".to_string()]);
        // engineer_member is a codex member without configured effort: medium -> high.
        assert_eq!(state.effort_override.as_deref(), Some("high"));
        assert!(!state.bench_reset_capped);

        let preserved = git_stdout(
            &repo,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/preserved",
            ],
        );
        assert!(
            preserved
                .lines()
                .any(|b| b.starts_with("preserved/eng-1-7-")),
            "{preserved}"
        );
        assert!(!bench_entries(&repo).unwrap().contains_key("eng-1"));
        assert_eq!(daemon.active_task_id("eng-1"), None);
        assert!(
            !daemon
                .verification
                .zero_diff_completion_counts
                .contains_key(&7)
        );
        assert!(event_names(&repo).iter().any(|e| e == "task_bench_reset"));
        assert_eq!(orchestrator_lines(&repo, "reset task #7"), 1);
    }

    #[test]
    fn second_reset_increments_count_and_effort_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, task_path, mut daemon) = parked_daemon(&tmp);

        daemon.reset_parked_task("eng-1", 7).unwrap();
        // eng-2 picked it up and was parked too.
        task_cmd::update_task_frontmatter(&task_path, |mapping| {
            task_cmd::set_optional_string(mapping, "status", Some("in-progress"));
            task_cmd::set_optional_string(mapping, "claimed_by", Some("eng-2"));
        })
        .unwrap();
        daemon.reset_parked_task("eng-2", 7).unwrap();

        let state = read_bench_recovery(&task_path);
        assert_eq!(state.bench_reset_count, 2);
        assert_eq!(
            state.avoid_engineers,
            vec!["eng-1".to_string(), "eng-2".to_string()]
        );
        assert_eq!(state.effort_override.as_deref(), Some("xhigh"));
    }

    #[test]
    fn reset_at_cap_blocks_escalates_and_unbenches() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, task_path, mut daemon) = parked_daemon(&tmp);
        write_bench_recovery(
            &task_path,
            &BenchRecoveryFrontmatter {
                bench_reset_count: 2,
                avoid_engineers: vec!["eng-2".to_string()],
                effort_override: Some("xhigh".to_string()),
                bench_reset_capped: false,
            },
        )
        .unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        daemon.delivery.channels.insert(
            "human".to_string(),
            Box::new(RecordingChannel {
                messages: Arc::clone(&sent),
            }),
        );

        daemon.reset_parked_task("eng-1", 7).unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "blocked");
        assert_eq!(task.blocked_on.as_deref(), Some(RESET_CAP_REASON));
        let state = read_bench_recovery(&task_path);
        assert!(state.bench_reset_capped);
        assert_eq!(state.bench_reset_count, 2);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].contains("Task #7 is blocked"), "{}", sent[0]);
        let events = event_names(&repo);
        assert!(events.iter().any(|e| e == "task_bench_reset_capped"));
        assert!(events.iter().any(|e| e == "task_escalated"));
        assert!(!events.iter().any(|e| e == "task_bench_reset"));
        assert!(!bench_entries(&repo).unwrap().contains_key("eng-1"));
        assert_eq!(daemon.active_task_id("eng-1"), None);
    }

    #[test]
    fn reset_of_review_task_writes_todo_despite_state_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, task_path, mut daemon) = parked_daemon(&tmp);
        task_cmd::update_task_frontmatter(&task_path, |mapping| {
            task_cmd::set_optional_string(mapping, "status", Some("review"));
        })
        .unwrap();

        daemon.reset_parked_task("eng-1", 7).unwrap();

        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(read_bench_recovery(&task_path).bench_reset_count, 1);
    }

    fn wait_for_bench_jobs(daemon: &mut TeamDaemon) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !daemon
            .verification
            .background_tests
            .bench_approvals
            .is_empty()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "bench approval job never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            daemon.poll_bench_approval_jobs();
        }
    }

    /// Task 7 in review from eng-1 with every engineer benched.
    fn review_daemon(
        tmp: &tempfile::TempDir,
        test_command: &str,
    ) -> (PathBuf, PathBuf, TeamDaemon) {
        let (repo, task_path, mut daemon) = parked_daemon(tmp);
        task_cmd::update_task_frontmatter(&task_path, |mapping| {
            task_cmd::set_optional_string(mapping, "status", Some("review"));
        })
        .unwrap();
        crate::team::bench::bench_engineer(&repo, "eng-2", Some("manual")).unwrap();
        daemon.config.team_config.workflow_policy.test_command = Some(test_command.to_string());
        (repo, task_path, daemon)
    }

    fn queued_origins(daemon: &TeamDaemon) -> Vec<(u32, MergeOrigin)> {
        daemon
            .merge
            .queue
            .snapshot()
            .queue
            .iter()
            .map(|request| (request.task_id, request.origin))
            .collect()
    }

    #[test]
    fn review_approval_enqueues_bench_recovery_merge_and_records_daemon_reviewer() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, task_path, mut daemon) = review_daemon(&tmp, "true");

        assert!(daemon.start_bench_review_approval(7).unwrap());
        wait_for_bench_jobs(&mut daemon);

        assert_eq!(
            queued_origins(&daemon),
            vec![(7, MergeOrigin::BenchRecovery)]
        );
        assert!(event_names(&repo).iter().any(|e| e == "task_auto_approved"));

        daemon.process_merge_queue_for_test().unwrap();
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "done");
        let content = std::fs::read_to_string(&task_path).unwrap();
        assert!(
            content.contains(crate::team::daemon::merge_queue::BENCH_RECOVERY_REVIEWER),
            "{content}"
        );
        assert!(
            content.contains("review_disposition: approved"),
            "{content}"
        );
    }

    #[test]
    fn review_approval_is_not_started_when_auto_merge_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, _task_path, mut daemon) = review_daemon(&tmp, "true");
        daemon.config.team_config.workflow_policy.auto_merge.enabled = false;

        assert!(!daemon.start_bench_review_approval(7).unwrap());
        assert!(
            daemon
                .verification
                .background_tests
                .bench_approvals
                .is_empty()
        );
        assert!(queued_origins(&daemon).is_empty());
    }

    #[test]
    fn review_approval_enqueues_nothing_after_an_unbench_mid_job() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, task_path, mut daemon) = review_daemon(&tmp, "true");

        assert!(daemon.start_bench_review_approval(7).unwrap());
        crate::team::bench::unbench_engineer(&repo, "eng-2").unwrap();
        wait_for_bench_jobs(&mut daemon);

        assert!(queued_origins(&daemon).is_empty());
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "review");
        assert_eq!(read_bench_recovery(&task_path).bench_reset_count, 0);
    }

    #[test]
    fn failed_review_verification_resets_the_task() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, task_path, mut daemon) = review_daemon(&tmp, "false");

        assert!(daemon.start_bench_review_approval(7).unwrap());
        wait_for_bench_jobs(&mut daemon);

        assert!(queued_origins(&daemon).is_empty());
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(read_bench_recovery(&task_path).bench_reset_count, 1);
    }

    #[test]
    fn skipped_verification_does_not_meet_the_bar() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, task_path, mut daemon) = review_daemon(&tmp, "true");
        daemon
            .config
            .team_config
            .workflow_policy
            .verification
            .auto_run_tests = false;

        assert!(daemon.start_bench_review_approval(7).unwrap());
        wait_for_bench_jobs(&mut daemon);

        assert!(queued_origins(&daemon).is_empty());
        assert_eq!(read_bench_recovery(&task_path).bench_reset_count, 1);
    }

    #[test]
    fn parked_task_meeting_the_bar_is_queued_and_engineer_unbenched() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, task_path, mut daemon) = parked_daemon(&tmp);
        daemon.config.team_config.workflow_policy.test_command = Some("true".to_string());

        assert!(daemon.start_parked_task_recovery("eng-1", 7).unwrap());
        wait_for_bench_jobs(&mut daemon);

        assert_eq!(
            queued_origins(&daemon),
            vec![(7, MergeOrigin::BenchRecovery)]
        );
        assert!(!bench_entries(&repo).unwrap().contains_key("eng-1"));
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "review");
    }

    #[test]
    fn parked_task_without_attributed_commits_is_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, task_path, mut daemon) = parked_daemon(&tmp);
        daemon.config.team_config.workflow_policy.test_command = Some("true".to_string());
        // Only a daemon auto-save on the task branch: not attributed work.
        let worktree = repo.join(".batty").join("worktrees").join("eng-1");
        git_ok(&worktree, &["reset", "--hard", "eng-main/eng-1"]);
        std::fs::write(worktree.join("wip.txt"), "wip\n").unwrap();
        git_ok(&worktree, &["add", "wip.txt"]);
        git_ok(
            &worktree,
            &["commit", "-m", "wip: auto-save before restart [batty]"],
        );

        assert!(daemon.start_parked_task_recovery("eng-1", 7).unwrap());
        wait_for_bench_jobs(&mut daemon);

        assert!(queued_origins(&daemon).is_empty());
        let task = crate::task::Task::from_file(&task_path).unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(read_bench_recovery(&task_path).bench_reset_count, 1);
    }

    #[test]
    fn bench_approval_is_refused_for_multi_repo_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, _task_path, mut daemon) = review_daemon(&tmp, "true");
        daemon.is_multi_repo = true;

        let error = daemon.start_bench_review_approval(7).unwrap_err();
        assert!(error.to_string().contains("multi-repo"), "{error}");
    }

    #[test]
    fn duplicate_bench_approval_job_is_not_started() {
        let tmp = tempfile::tempdir().unwrap();
        let (_repo, _task_path, mut daemon) = review_daemon(&tmp, "sleep 1");

        assert!(daemon.start_bench_review_approval(7).unwrap());
        assert!(!daemon.start_bench_review_approval(7).unwrap());
        assert_eq!(
            daemon.verification.background_tests.bench_approvals.len(),
            1
        );
        wait_for_bench_jobs(&mut daemon);
    }

    #[test]
    fn all_engineers_benched_needs_a_dispatchable_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = TestDaemonBuilder::new(tmp.path())
            .members(vec![manager_member("manager", None)])
            .build();
        assert!(!daemon.all_engineers_benched(&BTreeMap::new()));
    }
}
