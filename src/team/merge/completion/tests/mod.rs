//! Completion tests: shared fixtures here, tests grouped by the module
//! whose behaviour they cover.

mod decision;
mod execute;
mod gates;
mod multi_repo;
mod verification;

use super::decision::{
    analyze_completion_diff, move_task_to_review, record_completion_packet_metadata,
};
use super::multi_repo::{
    merge_multi_repo_engineer_branch, multi_repo_code_files_changed_from_trunk,
    multi_repo_diff_stat_from_trunk, multi_repo_files_changed_from_trunk,
};
use super::verification::{poll_verification_jobs, wait_for_verification_jobs};
use super::*;
use crate::shim::protocol::{Command, ShimState, socketpair};
use crate::team::artifact::read_test_timing_log;
use crate::team::board::{WorkflowMetadata, write_workflow_metadata};
use crate::team::config::AutoMergePolicy;
use crate::team::daemon::agent_handle::AgentHandle;
use crate::team::daemon::{MergeOrigin, MergeRequest};
use crate::team::events::read_events;
use crate::team::hierarchy::MemberInstance;
use crate::team::inbox;
use crate::team::merge::MergeOutcome;
use crate::team::merge::poll_deferred_direct_merges;
use crate::team::standup::MemberState;
use crate::team::task_loop::setup_engineer_worktree;
use crate::team::task_loop::{checkout_worktree_branch_from_trunk, engineer_base_branch_name};
use crate::team::test_helpers::make_test_daemon;
use crate::team::test_support::{
    engineer_member, git_ok, git_stdout, init_git_repo, manager_member,
};
use crate::team::verification::VerificationPhase;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn verification_snapshot_path(repo: &Path, task_id: u32, engineer: &str, attempt: u32) -> PathBuf {
    repo.join(".batty")
        .join("reports")
        .join("verification")
        .join("completion")
        .join(format!(
            "task-{task_id:03}-{engineer}-attempt-{attempt}.json"
        ))
}

fn write_task_file(project_root: &Path, id: u32, title: &str) {
    write_task_file_with_body(project_root, id, title, "Task description.\n");
}

fn write_scope_ack_team_config(project_root: &Path) {
    let team_config_dir = project_root.join(".batty").join("team_config");
    std::fs::create_dir_all(&team_config_dir).unwrap();
    std::fs::write(
        team_config_dir.join("team.yaml"),
        "name: scope-test\nroles:\n  - name: architect\n    role_type: architect\n    agent: claude\n    instances: 1\n  - name: manager\n    role_type: manager\n    agent: claude\n    instances: 1\n  - name: engineer\n    role_type: engineer\n    agent: codex\n    instances: 1\n",
    )
    .unwrap();
}

fn write_task_file_with_body(project_root: &Path, id: u32, title: &str, body: &str) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{id:03}-{title}.md")),
        format!(
            "---\nid: {id}\ntitle: {title}\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n\n{body}"
        ),
    )
    .unwrap();
}

fn engineer_worktree_paths(repo: &Path, engineer: &str) -> (PathBuf, PathBuf) {
    let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
    let team_config_dir = repo.join(".batty").join("team_config");
    (worktree_dir, team_config_dir)
}

fn setup_completion_daemon(repo: &Path, engineer: &str) -> crate::team::daemon::TeamDaemon {
    let members = vec![
        manager_member("manager", None),
        engineer_member(engineer, Some("manager"), true),
    ];
    make_test_daemon(repo, members)
}

fn current_head(repo: &Path) -> String {
    String::from_utf8(
        std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string()
}

fn seed_completion_packet_metadata(repo: &Path, task_id: u32, title: &str, worktree_dir: &Path) {
    let branch = current_worktree_branch(worktree_dir).unwrap();
    let commit = current_head(worktree_dir);
    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join(format!("{task_id:03}-{title}.md"));
    write_workflow_metadata(
        &task_path,
        &WorkflowMetadata {
            branch: Some(branch),
            worktree_path: Some(worktree_dir.to_string_lossy().into_owned()),
            commit: Some(commit),
            changed_paths: vec!["note.txt".to_string()],
            tests_run: Some(true),
            tests_passed: Some(true),
            test_results: None,
            artifacts: Vec::new(),
            outcome: Some("ready_for_review".to_string()),
            review_blockers: Vec::new(),
        },
    )
    .unwrap();
}

fn setup_rebase_conflict_repo(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, engineer);

    std::fs::write(repo.join("conflict.txt"), "original\n").unwrap();
    git_ok(&repo, &["add", "conflict.txt"]);
    git_ok(&repo, &["commit", "-m", "add conflict file"]);

    setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

    std::fs::write(worktree_dir.join("conflict.txt"), "engineer version\n").unwrap();
    git_ok(&worktree_dir, &["add", "conflict.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

    std::fs::write(repo.join("conflict.txt"), "main version\n").unwrap();
    git_ok(&repo, &["add", "conflict.txt"]);
    git_ok(&repo, &["commit", "-m", "main change"]);

    (tmp, repo, worktree_dir, team_config_dir)
}

fn setup_auto_merge_repo(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-auto-merge-test");
    write_task_file(&repo, 42, "auto-merge-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
    setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

    // Create a small change in the worktree. We intentionally do NOT
    // pre-seed completion packet metadata here: production code is
    // responsible for writing it during `handle_engineer_completion`
    // before the merge queue picks up the request. Tests that depend on
    // the metadata being already present should exercise the real
    // completion path rather than faking the seed.
    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    git_ok(&worktree_dir, &["add", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "add note"]);

    (tmp, repo, worktree_dir)
}

fn setup_auto_merge_repo_without_work(
    engineer: &str,
    task_title: &str,
) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-auto-merge-test");
    write_task_file(&repo, 42, task_title);

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
    setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

    (tmp, repo, worktree_dir)
}

fn commit_all(worktree_dir: &Path, message: &str) {
    git_ok(worktree_dir, &["add", "."]);
    git_ok(worktree_dir, &["commit", "-m", message]);
}

fn assert_auto_merge_decision(repo: &Path, expected_action: &str, expected_reason_fragment: &str) {
    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    let decision_event = events
        .iter()
        .find(|event| event.event == "auto_merge_decision_recorded")
        .expect("should record auto-merge decision");
    assert_eq!(decision_event.action_type.as_deref(), Some(expected_action));
    assert!(
        decision_event
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(expected_reason_fragment)),
        "decision reason should contain `{expected_reason_fragment}`: {:?}",
        decision_event.reason
    );
}

fn setup_failing_test_repo(engineer: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-failing-test");
    write_task_file(&repo, 42, "failing-test-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
    setup_engineer_worktree(&repo, &worktree_dir, engineer, &team_config_dir).unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn test_dispatch_wip_guard() {\n        assert_eq!(2, 3);\n    }\n}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "add failing test"]);

    (tmp, repo, worktree_dir)
}

fn auto_merge_daemon(repo: &Path, policy: AutoMergePolicy) -> crate::team::daemon::TeamDaemon {
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), true),
    ];
    let mut daemon = make_test_daemon(repo, members);
    daemon.config.team_config.workflow_policy.auto_merge = policy;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    daemon
}

fn complete_task_and_process_queue(daemon: &mut crate::team::daemon::TeamDaemon, engineer: &str) {
    handle_engineer_completion_and_wait(daemon, engineer).unwrap();
    daemon.process_merge_queue_for_test().unwrap();
}

#[test]
fn completion_routes_engineers_with_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng-1".to_string(),
        role_type: crate::team::config::RoleType::Engineer,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);

    daemon.set_active_task_for_test("eng-1", 42);
    complete_task_and_process_queue(&mut daemon, "eng-1");
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
}

fn set_task_status(repo: &Path, file: &str, from: &str, to: &str) {
    let path = repo.join(".batty/team_config/board/tasks").join(file);
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replacen(&format!("status: {from}"), &format!("status: {to}"), 1),
    )
    .unwrap();
}

#[test]
fn completion_on_done_task_is_noop_and_clears_assignment() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "already-done");
    set_task_status(&repo, "042-already-done.md", "in-progress", "done");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    handle_engineer_completion(&mut daemon, "eng-1").unwrap();

    assert!(daemon.active_task_id("eng-1").is_none());
    let task =
        crate::task::Task::from_file(&team_config_dir.join("board/tasks/042-already-done.md"))
            .unwrap();
    assert_eq!(task.status, "done");
}

/// Repo + daemon whose completion verification runs `test_command`
/// (typically a slow `sleep`) for task #42 owned by eng-1.
fn setup_background_verification(
    test_command: &str,
) -> (tempfile::TempDir, PathBuf, crate::team::daemon::TeamDaemon) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-bg-verify");
    write_task_file(&repo, 42, "background-verification");
    let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    git_ok(&worktree_dir, &["add", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "add note"]);
    seed_completion_packet_metadata(&repo, 42, "background-verification", &worktree_dir);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .test_command = Some(test_command.to_string());
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    (tmp, repo, daemon)
}

fn init_sub_repo(project: &Path, name: &str) -> PathBuf {
    let repo = project.join(name);
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q", "-b", "main"]);
    git_ok(&repo, &["config", "user.email", "batty@example.com"]);
    git_ok(&repo, &["config", "user.name", "Batty Tests"]);
    std::fs::write(repo.join("shared.txt"), "base\n").unwrap();
    git_ok(&repo, &["add", "shared.txt"]);
    git_ok(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

/// Engineer worktree for `name` at `.batty/worktrees/eng-1/<name>` on a
/// task branch, optionally with one commit editing `shared.txt`.
fn add_sub_worktree(project: &Path, name: &str, edit: Option<&str>) -> PathBuf {
    let repo = project.join(name);
    let wt = project
        .join(".batty")
        .join("worktrees")
        .join("eng-1")
        .join(name);
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "eng-1/5",
            wt.to_str().unwrap(),
            "main",
        ],
    );
    if let Some(content) = edit {
        std::fs::write(wt.join("shared.txt"), content).unwrap();
        git_ok(&wt, &["commit", "-q", "-am", "engineer work"]);
    }
    wt
}

fn head_of(repo: &Path, rev: &str) -> String {
    git_stdout(repo, &["rev-parse", rev])
}

fn production_unwrap_expect_count(source: &str) -> usize {
    let prod = if let Some(pos) = source.find("\n#[cfg(test)]\nmod tests") {
        &source[..pos]
    } else {
        source
    };
    prod.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("#[cfg(test)]")
                && (trimmed.contains(".unwrap(") || trimmed.contains(".expect("))
        })
        .count()
}

#[test]
fn production_merge_completion_has_no_unwrap_or_expect_calls() {
    let sources = [
        ("mod.rs", include_str!("../mod.rs")),
        ("decision.rs", include_str!("../decision.rs")),
        ("execute.rs", include_str!("../execute.rs")),
        ("gates.rs", include_str!("../gates.rs")),
        ("messaging.rs", include_str!("../messaging.rs")),
        ("multi_repo.rs", include_str!("../multi_repo.rs")),
        ("verification.rs", include_str!("../verification.rs")),
    ];
    for (name, src) in sources {
        assert_eq!(
            production_unwrap_expect_count(src),
            0,
            "production completion/{name} should avoid unwrap/expect"
        );
    }
}
