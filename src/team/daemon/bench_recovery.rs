//! Bench recovery: hand parked work on, reset it onto another engineer at a
//! higher effort, or block it once the reset cap is reached.
//!
//! The persisted per-task state (reset count, avoid list, effort override)
//! lives in task frontmatter via `team::bench_recovery`. Bench-file access
//! always goes through the short-lived bench lock; git, messaging and board
//! writes here never run inside it.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, warn};

use super::TeamDaemon;
use crate::team::bench_recovery::{
    BenchRecoveryFrontmatter, effort_ladder_for_backend, next_effort, read_bench_recovery,
    write_bench_recovery,
};
use crate::team::config::RoleType;
use crate::team::events::TeamEvent;
use crate::team::task_cmd::{self, StatusTransitionAttribution};

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
    pub fn reset_parked_task(&mut self, engineer: &str, task_id: u32) -> Result<()> {
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
}
