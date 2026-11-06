use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::task;

use super::super::config::RoleType;
use super::super::daemon::{MainSmokeState, NudgeSchedule};
use super::super::daemon_mgmt::{
    daemon_child_pid_path, daemon_log_path, watchdog_pid_path, watchdog_state_path,
};
use super::super::hierarchy::MemberInstance;
use super::super::standup::MemberState;
use super::super::supervisory_notice::SupervisoryPressureSnapshot;
use super::super::{daemon_state_path, now_unix, team_events_path};
use super::*;

use rusqlite::Connection;
use std::fs;

use crate::team::events::{EventSink, TeamEvent};
use crate::team::inbox::InboxMessage;

fn engineer(name: &str) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: name.to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
    }
}

fn manager(name: &str) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: name.to_string(),
        role_type: RoleType::Manager,
        agent: Some("codex".to_string()),
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
    }
}

fn architect(name: &str) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: name.to_string(),
        role_type: RoleType::Architect,
        agent: Some("codex".to_string()),
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: None,
        use_worktrees: false,
    }
}

fn user_member(name: &str) -> MemberInstance {
    MemberInstance {
        name: name.to_string(),
        role_name: name.to_string(),
        role_type: RoleType::User,
        agent: None,
        model: None,
        prompt: None,
        posture: None,
        model_class: None,
        provider_overlay: None,
        reports_to: None,
        use_worktrees: false,
    }
}

fn board_dir(project_root: &Path) -> std::path::PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("board")
}

fn write_board_task(project_root: &Path, filename: &str, frontmatter: &str) {
    let tasks_dir = board_dir(project_root).join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join(filename),
        format!("---\n{frontmatter}class: standard\n---\n"),
    )
    .unwrap();
}

fn create_legacy_telemetry_db(project_root: &Path) -> Connection {
    let batty_dir = project_root.join(".batty");
    fs::create_dir_all(&batty_dir).unwrap();
    let conn = Connection::open(batty_dir.join("telemetry.db")).unwrap();
    crate::team::telemetry_db::install_legacy_schema_for_tests(&conn).unwrap();
    conn
}

#[test]
fn build_team_status_rows_marks_user_and_stopped_session() {
    let members = vec![engineer("eng-1"), user_member("human")];
    let rows = build_team_status_rows(
        &members,
        false,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(rows[0].state, "stopped");
    assert_eq!(rows[0].runtime_label, None);
    assert_eq!(rows[1].state, "user");
    assert_eq!(rows[1].role_type, "User");
    assert_eq!(rows[1].agent, None);
}

#[test]
fn build_team_status_rows_promotes_idle_member_with_triage_backlog() {
    let members = vec![manager("manager")];
    let runtime_statuses = HashMap::from([(
        "manager".to_string(),
        RuntimeMemberStatus {
            state: "idle".to_string(),
            signal: None,
            label: Some("idle".to_string()),
        },
    )]);
    let triage_backlog_counts = HashMap::from([("manager".to_string(), 2usize)]);
    let supervisory_pressure = HashMap::from([("manager".to_string(), {
        let mut snapshot = SupervisoryPressureSnapshot::default();
        snapshot.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::TriageBacklog,
            2,
        );
        snapshot
    })]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &triage_backlog_counts,
        &HashMap::new(),
        &supervisory_pressure,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(rows[0].state, "triaging");
    assert_eq!(rows[0].pending_inbox, 1);
    assert_eq!(
        rows[0].signal.as_deref(),
        Some("pressure 1: direct-report packets (2)")
    );
}

#[test]
fn delivered_direct_report_triage_state_dedupes_repeated_delivered_results() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
    crate::team::inbox::init_inbox(&inbox_root, "lead").unwrap();
    crate::team::inbox::init_inbox(&inbox_root, "eng-1").unwrap();

    let mut first = InboxMessage::new_send("eng-1", "lead", "first result");
    first.timestamp = now_unix().saturating_sub(30);
    let first_id = crate::team::inbox::deliver_to_inbox(&inbox_root, &first).unwrap();
    crate::team::inbox::mark_delivered(&inbox_root, "lead", &first_id).unwrap();

    let mut second = InboxMessage::new_send("eng-1", "lead", "second result");
    second.timestamp = now_unix().saturating_sub(10);
    let second_id = crate::team::inbox::deliver_to_inbox(&inbox_root, &second).unwrap();
    crate::team::inbox::mark_delivered(&inbox_root, "lead", &second_id).unwrap();

    let state =
        delivered_direct_report_triage_state(&inbox_root, "lead", &["eng-1".to_string()]).unwrap();
    assert_eq!(state.count, 1);
    assert_eq!(state.newest_result_ts, second.timestamp);
}

#[test]
fn pending_inbox_counts_for_supervisors_ignore_stale_status_rollups() {
    let tmp = tempfile::tempdir().unwrap();
    let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
    crate::team::inbox::init_inbox(&inbox_root, "manager").unwrap();

    crate::team::inbox::deliver_to_inbox(
        &inbox_root,
        &InboxMessage::new_send(
            "daemon",
            "manager",
            "Rollup: review backlog is healthy and no action is required right now.",
        ),
    )
    .unwrap();
    crate::team::inbox::deliver_to_inbox(
        &inbox_root,
        &InboxMessage::new_send(
            "daemon",
            "manager",
            "Dispatch queue entry failed validation too many times.",
        ),
    )
    .unwrap();

    let counts = pending_inbox_counts(tmp.path(), &[manager("manager")]);
    assert_eq!(counts.get("manager"), Some(&1usize));
}

#[test]
fn build_team_status_rows_promotes_idle_member_with_review_backlog() {
    let members = vec![manager("manager")];
    let runtime_statuses = HashMap::from([(
        "manager".to_string(),
        RuntimeMemberStatus {
            state: "idle".to_string(),
            signal: Some("nudge paused".to_string()),
            label: Some("idle".to_string()),
        },
    )]);
    let owned_task_buckets = HashMap::from([(
        "manager".to_string(),
        OwnedTaskBuckets {
            active: Vec::new(),
            review: vec![41, 42],
            stale_review: Vec::new(),
        },
    )]);
    let supervisory_pressure = HashMap::from([("manager".to_string(), {
        let mut snapshot = SupervisoryPressureSnapshot::default();
        snapshot.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
            2,
        );
        snapshot
    })]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &HashMap::new(),
        &owned_task_buckets,
        &supervisory_pressure,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(rows[0].state, "reviewing");
    assert_eq!(
        rows[0].signal.as_deref(),
        Some("nudge paused, pressure 1: review backlog (2)")
    );
}

#[test]
fn build_team_status_rows_distinguishes_stale_review_backlog() {
    let members = vec![manager("manager")];
    let runtime_statuses = HashMap::from([(
        "manager".to_string(),
        RuntimeMemberStatus {
            state: "idle".to_string(),
            signal: None,
            label: Some("idle".to_string()),
        },
    )]);
    let owned_task_buckets = HashMap::from([(
        "manager".to_string(),
        OwnedTaskBuckets {
            active: Vec::new(),
            review: vec![41],
            stale_review: vec![42],
        },
    )]);
    let supervisory_pressure = HashMap::from([("manager".to_string(), {
        let mut snapshot = SupervisoryPressureSnapshot::default();
        snapshot.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
            1,
        );
        snapshot
    })]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &HashMap::new(),
        &owned_task_buckets,
        &supervisory_pressure,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(rows[0].state, "reviewing");
    assert_eq!(
        rows[0].signal.as_deref(),
        Some("pressure 1: review backlog (1), stale review (1)")
    );
}

#[test]
fn build_team_status_rows_surfaces_supervisory_stall_signal_and_role_token() {
    let members = vec![architect("architect"), manager("manager")];
    let runtime_statuses = HashMap::from([
        (
            "architect".to_string(),
            RuntimeMemberStatus {
                state: "working".to_string(),
                signal: None,
                label: Some("working".to_string()),
            },
        ),
        (
            "manager".to_string(),
            RuntimeMemberStatus {
                state: "working".to_string(),
                signal: Some("nudge paused".to_string()),
                label: Some("working".to_string()),
            },
        ),
    ]);
    let agent_health = HashMap::from([
        (
            "architect".to_string(),
            AgentHealthSummary {
                stall_reason: Some(
                    "supervisory_stalled_architect_no_actionable_progress".to_string(),
                ),
                stall_summary: Some(
                    "architect (architect) stalled after 5m: no actionable progress".to_string(),
                ),
                ..AgentHealthSummary::default()
            },
        ),
        (
            "manager".to_string(),
            AgentHealthSummary {
                stall_reason: Some("supervisory_stalled_manager_shim_activity_only".to_string()),
                stall_summary: Some(
                    "manager (manager) stalled after 5m: shim activity only".to_string(),
                ),
                ..AgentHealthSummary::default()
            },
        ),
    ]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &agent_health,
    );

    assert_eq!(rows[0].health_summary, "stall:architect:working-timeout");
    assert_eq!(
        rows[0].signal.as_deref(),
        Some("architect (architect) stalled after 5m: no actionable progress")
    );
    assert_eq!(rows[1].health_summary, "stall:manager:working-timeout");
    assert_eq!(
        rows[1].signal.as_deref(),
        Some("nudge paused, manager (manager) stalled after 5m: shim activity only")
    );
}

#[test]
fn build_team_status_rows_keeps_actionable_control_plane_stall_with_pressure() {
    let members = vec![manager("manager")];
    let runtime_statuses = HashMap::from([(
        "manager".to_string(),
        RuntimeMemberStatus {
            state: "working".to_string(),
            signal: Some("nudge paused".to_string()),
            label: Some("working".to_string()),
        },
    )]);
    let supervisory_pressure = HashMap::from([("manager".to_string(), {
        let mut snapshot = SupervisoryPressureSnapshot::default();
        snapshot.add_pressure(
            crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
            2,
        );
        snapshot
    })]);
    let agent_health = HashMap::from([(
        "manager".to_string(),
        AgentHealthSummary {
            stall_reason: Some("supervisory_stalled_manager_review_backlog".to_string()),
            stall_summary: Some(
                "manager (manager) stalled after 5m: stale review backlog; next: review and disposition queued work"
                    .to_string(),
            ),
            ..AgentHealthSummary::default()
        },
    )]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &supervisory_pressure,
        &HashMap::new(),
        &HashMap::new(),
        &agent_health,
    );

    assert_eq!(rows[0].state, "working");
    assert_eq!(rows[0].health_summary, "stall:manager:review-wait-timeout");
    assert_eq!(
        rows[0].signal.as_deref(),
        Some(
            "nudge paused, manager (manager) stalled after 5m: stale review backlog; next: review and disposition queued work, pressure 1: review backlog (2)"
        )
    );
}

#[test]
fn build_team_status_rows_defaults_to_starting_when_runtime_missing() {
    let members = vec![engineer("eng-1")];
    let rows = build_team_status_rows(
        &members,
        true,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(rows[0].state, "starting");
    assert_eq!(rows[0].runtime_label, None);
}

#[test]
fn build_team_status_rows_surfaces_cleared_stale_active_lane() {
    let members = vec![engineer("eng-1")];
    let runtime_statuses = HashMap::from([(
        "eng-1".to_string(),
        RuntimeMemberStatus {
            state: "idle".to_string(),
            signal: None,
            label: None,
        },
    )]);
    let agent_health = HashMap::from([(
        "eng-1".to_string(),
        AgentHealthSummary {
            stale_active_cleared_count: 1,
            stale_active_summary: Some("cleared stale active #42 (done)".to_string()),
            ..AgentHealthSummary::default()
        },
    )]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &agent_health,
    );

    assert_eq!(rows[0].state, "idle");
    assert_eq!(
        rows[0].signal.as_deref(),
        Some("cleared stale active #42 (done)")
    );
    assert_eq!(rows[0].health_summary, "sa1");
}

#[test]
fn build_team_status_rows_includes_branch_mismatch_signal() {
    let members = vec![engineer("eng-1")];
    let runtime_statuses = HashMap::from([(
        "eng-1".to_string(),
        RuntimeMemberStatus {
            state: "idle".to_string(),
            signal: Some("nudge paused".to_string()),
            label: Some("idle".to_string()),
        },
    )]);
    let owned_task_buckets = HashMap::from([(
        "eng-1".to_string(),
        OwnedTaskBuckets {
            active: vec![41],
            review: Vec::new(),
            stale_review: Vec::new(),
        },
    )]);
    let branch_mismatches = HashMap::from([(
        "eng-1".to_string(),
        "branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
            .to_string(),
    )]);

    let rows = build_team_status_rows(
        &members,
        true,
        &runtime_statuses,
        &HashMap::new(),
        &HashMap::new(),
        &owned_task_buckets,
        &HashMap::new(),
        &branch_mismatches,
        &HashMap::new(),
        &HashMap::new(),
    );

    assert_eq!(
        rows[0].signal.as_deref(),
        Some(
            "nudge paused, branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
        )
    );
}

#[test]
fn branch_mismatch_by_member_flags_detached_head_claimed_task() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-branch-mismatch");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/41").unwrap();
    let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    crate::team::test_support::git_ok(&worktree_dir, &["checkout", "--detach", &head]);

    write_board_task(
        &repo,
        "041-active.md",
        "id: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\n",
    );

    let mut member = engineer("eng-1");
    member.use_worktrees = true;
    let mismatches = branch_mismatch_by_member(&repo, &[member]);

    assert_eq!(
        mismatches.get("eng-1").map(String::as_str),
        Some(
            "branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
        )
    );
}

#[test]
fn branch_mismatch_by_member_ignores_matching_claimed_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-branch-match");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/41").unwrap();

    write_board_task(
        &repo,
        "041-active.md",
        "id: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\n",
    );

    let mut member = engineer("eng-1");
    member.use_worktrees = true;
    let mismatches = branch_mismatch_by_member(&repo, &[member]);

    assert!(mismatches.is_empty());
}

#[test]
fn branch_mismatch_by_member_surfaces_preserved_completed_lane_signal() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-preserved-done");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();

    let record = crate::team::checkpoint::PreservedLaneRecord::commit(
        "eng-1",
        &task::Task {
            id: 628,
            title: "done lane".to_string(),
            status: "done".to_string(),
            priority: "high".to_string(),
            assignee: None,
            claimed_by: Some("eng-1".to_string()),
            claimed_at: None,
            claim_ttl_secs: None,
            claim_expires_at: None,
            last_progress_at: None,
            claim_warning_sent_at: None,
            claim_extensions: None,
            last_output_bytes: None,
            blocked: None,
            tags: vec![],
            depends_on: vec![],
            review_owner: None,
            blocked_on: None,
            worktree_path: Some(".batty/worktrees/eng-1".to_string()),
            branch: Some("eng-1/628".to_string()),
            commit: None,
            artifacts: vec![],
            next_action: None,
            scheduled_for: None,
            cron_schedule: None,
            cron_last_run: None,
            completed: None,
            description: "done".to_string(),
            batty_config: None,
            source_path: repo
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks")
                .join("628-done.md"),
        },
        "eng-1/628",
        &base_branch,
        "completed task no longer needs engineer lane",
        Some("abc1234567890".to_string()),
        "def4567890abc".to_string(),
    );
    crate::team::checkpoint::write_preserved_lane_record(&repo, &record).unwrap();

    let mut member = engineer("eng-1");
    member.use_worktrees = true;
    let mismatches = branch_mismatch_by_member(&repo, &[member]);

    assert_eq!(
        mismatches.get("eng-1").map(String::as_str),
        Some("saved completed lane #628 before cleanup (commit eng-1/628 @ def4567890ab)")
    );
}

#[test]
fn branch_mismatch_by_member_ignores_detached_head_without_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-detached-no-claim");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();
    let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    crate::team::test_support::git_ok(&worktree_dir, &["checkout", "--detach", &head]);

    let mut member = engineer("eng-1");
    member.use_worktrees = true;
    let mismatches = branch_mismatch_by_member(&repo, &[member]);

    assert!(mismatches.is_empty());
}

#[test]
fn status_worktree_repo_targets_discovers_git_children_under_non_git_worktree_root() {
    let tmp = tempfile::tempdir().unwrap();
    let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1");
    let sub_repo = worktree_dir.join("repo");
    fs::create_dir_all(&sub_repo).unwrap();
    crate::team::test_support::git_ok(&sub_repo, &["init", "-b", "main"]);
    crate::team::test_support::git_ok(&sub_repo, &["config", "user.email", "batty@example.com"]);
    crate::team::test_support::git_ok(&sub_repo, &["config", "user.name", "Batty Tests"]);
    fs::write(sub_repo.join("README.md"), "hello\n").unwrap();
    crate::team::test_support::git_ok(&sub_repo, &["add", "README.md"]);
    crate::team::test_support::git_ok(&sub_repo, &["commit", "-m", "initial"]);

    let targets = status_worktree_repo_targets(&worktree_dir);

    assert_eq!(
        targets,
        vec![StatusWorktreeRepoTarget {
            label: Some("repo".to_string()),
            path: sub_repo,
        }]
    );
}

#[test]
fn worktree_staleness_by_member_uses_subrepos_for_multi_repo_worktrees() {
    let tmp = tempfile::tempdir().unwrap();
    let worktree_dir = tmp.path().join(".batty").join("worktrees").join("eng-1");
    let sub_repo = worktree_dir.join("repo");
    fs::create_dir_all(&sub_repo).unwrap();
    crate::team::test_support::git_ok(&sub_repo, &["init", "-b", "main"]);
    crate::team::test_support::git_ok(&sub_repo, &["config", "user.email", "batty@example.com"]);
    crate::team::test_support::git_ok(&sub_repo, &["config", "user.name", "Batty Tests"]);
    fs::write(sub_repo.join("README.md"), "base\n").unwrap();
    crate::team::test_support::git_ok(&sub_repo, &["add", "README.md"]);
    crate::team::test_support::git_ok(&sub_repo, &["commit", "-m", "initial"]);
    crate::team::test_support::git_ok(&sub_repo, &["checkout", "-b", "feature"]);
    crate::team::test_support::git_ok(&sub_repo, &["checkout", "main"]);
    fs::write(sub_repo.join("README.md"), "main update\n").unwrap();
    crate::team::test_support::git_ok(&sub_repo, &["commit", "-am", "main update"]);
    crate::team::test_support::git_ok(&sub_repo, &["checkout", "feature"]);

    let mut member = engineer("eng-1");
    member.use_worktrees = true;

    let staleness = worktree_staleness_by_member(tmp.path(), &[member]);

    assert_eq!(staleness.get("eng-1"), Some(&1));
}

#[test]
fn build_team_status_health_counts_non_user_members_and_sorts_unhealthy() {
    let rows = vec![
        TeamStatusRow {
            name: "human".to_string(),
            role: "user".to_string(),
            role_type: "User".to_string(),
            agent: None,
            reports_to: None,
            state: "user".to_string(),
            pending_inbox: 9,
            triage_backlog: 9,
            active_owned_tasks: Vec::new(),
            review_owned_tasks: Vec::new(),
            signal: None,
            runtime_label: None,
            worktree_staleness: None,
            health: AgentHealthSummary::default(),
            health_summary: "-".to_string(),
            eta: "-".to_string(),
        },
        TeamStatusRow {
            name: "eng-2".to_string(),
            role: "engineer".to_string(),
            role_type: "Engineer".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("manager".to_string()),
            state: "working".to_string(),
            pending_inbox: 1,
            triage_backlog: 2,
            active_owned_tasks: vec![2],
            review_owned_tasks: Vec::new(),
            signal: None,
            runtime_label: Some("working".to_string()),
            worktree_staleness: None,
            health: AgentHealthSummary {
                restart_count: 1,
                context_exhaustion_count: 0,
                proactive_handoff_count: 0,
                delivery_failure_count: 0,
                supervisory_digest_count: 0,
                dispatch_fallback_count: 0,
                dispatch_fallback_reason: None,
                stale_active_cleared_count: 0,
                stale_active_summary: None,
                task_elapsed_secs: None,
                backend_health: crate::agent::BackendHealth::default(),
                stall_summary: None,
                stall_reason: None,
            },
            health_summary: "r1".to_string(),
            eta: "-".to_string(),
        },
        TeamStatusRow {
            name: "eng-1".to_string(),
            role: "engineer".to_string(),
            role_type: "Engineer".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("manager".to_string()),
            state: "reviewing".to_string(),
            pending_inbox: 3,
            triage_backlog: 1,
            active_owned_tasks: Vec::new(),
            review_owned_tasks: vec![1],
            signal: None,
            runtime_label: Some("idle".to_string()),
            worktree_staleness: None,
            health: AgentHealthSummary {
                restart_count: 0,
                context_exhaustion_count: 1,
                proactive_handoff_count: 0,
                delivery_failure_count: 1,
                supervisory_digest_count: 0,
                dispatch_fallback_count: 0,
                dispatch_fallback_reason: None,
                stale_active_cleared_count: 0,
                stale_active_summary: None,
                task_elapsed_secs: None,
                backend_health: crate::agent::BackendHealth::default(),
                stall_summary: None,
                stall_reason: None,
            },
            health_summary: "c1 d1".to_string(),
            eta: "-".to_string(),
        },
    ];

    let health = build_team_status_health(&rows, true, false);
    assert_eq!(health.member_count, 2);
    assert_eq!(health.active_member_count, 2);
    assert_eq!(health.pending_inbox_count, 4);
    assert_eq!(health.triage_backlog_count, 3);
    assert_eq!(
        health.unhealthy_members,
        vec!["eng-1".to_string(), "eng-2".to_string()]
    );
}

#[test]
fn board_status_task_queues_returns_empty_when_board_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert!(active_tasks.is_empty());
    assert!(review_queue.is_empty());
}

#[test]
fn owned_task_buckets_routes_review_items_to_manager() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "003-review.md",
        "id: 3\ntitle: Review one\nstatus: review\npriority: high\nclaimed_by: eng-2\n",
    );
    write_board_task(
        tmp.path(),
        "004-review.md",
        "id: 4\ntitle: Review two\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
    );
    write_board_task(
        tmp.path(),
        "005-active.md",
        "id: 5\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-2\n",
    );

    let buckets = owned_task_buckets(
        tmp.path(),
        &[manager("manager"), engineer("eng-1"), engineer("eng-2")],
    );

    assert_eq!(
        buckets.get("manager"),
        Some(&OwnedTaskBuckets {
            active: Vec::new(),
            review: vec![4],
            stale_review: vec![3],
        })
    );
    assert_eq!(
        buckets.get("eng-2"),
        Some(&OwnedTaskBuckets {
            active: vec![5],
            review: Vec::new(),
            stale_review: Vec::new(),
        })
    );
}

#[test]
fn compute_metrics_returns_default_when_board_is_missing() {
    let metrics = compute_metrics(&tempfile::tempdir().unwrap().path().join("board"), &[]).unwrap();

    assert_eq!(metrics, WorkflowMetrics::default());
}

#[test]
fn compute_metrics_returns_default_when_board_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(board_dir(tmp.path()).join("tasks")).unwrap();

    let metrics = compute_metrics(&board_dir(tmp.path()), &[]).unwrap();
    assert_eq!(metrics, WorkflowMetrics::default());
    assert_eq!(metrics.board_state, WorkflowBoardState::EmptyBoard);
}

#[test]
fn compute_metrics_counts_workflow_states_and_idle_runnable() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "001-runnable.md",
        "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
    );
    write_board_task(
        tmp.path(),
        "002-blocked.md",
        "id: 2\ntitle: Blocked\nstatus: blocked\npriority: medium\n",
    );
    write_board_task(
        tmp.path(),
        "003-review.md",
        "id: 3\ntitle: Review\nstatus: review\npriority: medium\nclaimed_by: eng-1\n",
    );
    write_board_task(
        tmp.path(),
        "004-in-progress.md",
        "id: 4\ntitle: In progress\nstatus: in-progress\npriority: medium\nclaimed_by: eng-1\n",
    );
    write_board_task(
        tmp.path(),
        "005-claimed.md",
        "id: 5\ntitle: Claimed todo\nstatus: todo\npriority: low\nclaimed_by: eng-3\n",
    );
    write_board_task(
        tmp.path(),
        "006-waiting.md",
        "id: 6\ntitle: Waiting\nstatus: todo\npriority: low\ndepends_on:\n  - 7\n",
    );
    write_board_task(
        tmp.path(),
        "007-parent.md",
        "id: 7\ntitle: Parent\nstatus: in-progress\npriority: low\nclaimed_by: eng-3\n",
    );

    let inbox_root = crate::team::inbox::inboxes_root(tmp.path());
    crate::team::inbox::deliver_to_inbox(
        &inbox_root,
        &InboxMessage::new_send("manager", "eng-2", "please pick this up"),
    )
    .unwrap();

    let metrics = compute_metrics(
        &board_dir(tmp.path()),
        &[
            engineer("eng-1"),
            engineer("eng-2"),
            engineer("eng-3"),
            engineer("eng-4"),
        ],
    )
    .unwrap();

    assert_eq!(metrics.board_state, WorkflowBoardState::RunnableBoard);
    assert_eq!(metrics.runnable_count, 1);
    assert_eq!(metrics.implementation_runnable_count, 1);
    assert_eq!(metrics.blocked_count, 1);
    assert_eq!(metrics.in_review_count, 1);
    assert_eq!(metrics.actionable_review_count, 1);
    assert_eq!(metrics.in_progress_count, 2);
    assert_eq!(metrics.idle_with_runnable, vec!["eng-4".to_string()]);
    assert_eq!(
        metrics.blocked_dispatch_reasons,
        vec![
            "#5 Claimed todo: claimed by eng-3; next: release stale claim or normalize matching active work to in-progress".to_string(),
            "#6 Waiting: unmet dependency #7 (in-progress)".to_string(),
        ]
    );
    assert_eq!(
        metrics.blocked_task_summaries,
        vec![
            "#2 Blocked: blocked without recorded reason Next: record an explicit owner/command, then move to todo when executable".to_string()
        ]
    );
    assert!(metrics.oldest_review_age_secs.is_some());
    assert!(metrics.oldest_assignment_age_secs.is_some());
}

#[test]
fn idle_with_runnable_excludes_non_engineer_owned_runnable() {
    // Regression for #711 — when the only runnable task is body-owned by
    // the architect, compute_idle_with_runnable must not list engineers
    // as "idle with runnable" because they cannot dispatch it.
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board_dir(tmp.path()).join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    // Maya-owned strategy task — runnable but not engineer-runnable.
    fs::write(
        tasks_dir.join("001-maya-strategy.md"),
        "---\nid: 1\ntitle: Maya strategy\nstatus: todo\npriority: high\nclass: standard\n---\n\n**Owner:** maya-lead (architect strategy work)\n",
    )
    .unwrap();

    let metrics = compute_metrics(
        &board_dir(tmp.path()),
        &[architect("maya-lead"), engineer("eng-1"), engineer("eng-2")],
    )
    .unwrap();

    // runnable_count still reports 1 — the task IS dispatchable in general.
    assert_eq!(metrics.runnable_count, 1);
    assert_eq!(metrics.implementation_runnable_count, 0);
    // But no engineer should be flagged as idle-with-runnable because
    // no engineer-dispatchable task exists.
    assert!(
        metrics.idle_with_runnable.is_empty(),
        "expected no idle-with-runnable engineers, got {:?}",
        metrics.idle_with_runnable
    );
}

#[test]
fn idle_with_runnable_lists_engineers_when_engineer_task_available() {
    // Companion test for #711 — when at least one engineer-dispatchable
    // task exists, idle engineers are still flagged.
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board_dir(tmp.path()).join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    // Maya-owned (non-engineer) — filtered out.
    fs::write(
        tasks_dir.join("001-maya-strategy.md"),
        "---\nid: 1\ntitle: Maya strategy\nstatus: todo\npriority: high\nclass: standard\n---\n\n**Owner:** maya-lead\n",
    )
    .unwrap();
    // Plain engineer-runnable task — keeps idle_with_runnable non-empty.
    fs::write(
        tasks_dir.join("002-engineer-todo.md"),
        "---\nid: 2\ntitle: Engineer work\nstatus: todo\npriority: medium\nclass: standard\n---\n\nRegular engineer work.\n",
    )
    .unwrap();

    let metrics = compute_metrics(
        &board_dir(tmp.path()),
        &[architect("maya-lead"), engineer("eng-1")],
    )
    .unwrap();

    assert_eq!(metrics.runnable_count, 2);
    assert_eq!(metrics.implementation_runnable_count, 1);
    assert_eq!(metrics.idle_with_runnable, vec!["eng-1".to_string()]);
}

#[test]
fn format_metrics_distinguishes_no_work_from_review_bottleneck() {
    let no_work = format_metrics(&WorkflowMetrics::default());
    assert!(no_work.contains("Board State: empty-board"));
    assert!(no_work.contains("Implementation Work: no executable implementation work"));

    let review_bottleneck = format_metrics(&WorkflowMetrics {
        board_state: WorkflowBoardState::ReviewBacklogGated,
        in_review_count: 1,
        actionable_review_count: 1,
        ..WorkflowMetrics::default()
    });
    assert!(review_bottleneck.contains("Board State: review-backlog-gated"));
    assert!(review_bottleneck.contains("Implementation Work: review backlog is the bottleneck"));

    let runnable = format_metrics(&WorkflowMetrics {
        board_state: WorkflowBoardState::RunnableBoard,
        runnable_count: 1,
        implementation_runnable_count: 1,
        actionable_review_count: 1,
        ..WorkflowMetrics::default()
    });
    assert!(runnable.contains("Board State: runnable-board"));
    assert!(runnable.contains("Implementation Work: executable implementation work available"));
}

#[test]
fn compute_metrics_distinguishes_blocked_only_board() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "737-blocked.md",
        "id: 737\ntitle: Blocked only\nstatus: blocked\npriority: high\nblocked_on: missing unblock task\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.board_state, WorkflowBoardState::BlockedOnlyBoard);
    assert_eq!(metrics.runnable_count, 0);
    assert_eq!(metrics.blocked_count, 1);
    assert!(format_metrics(&metrics).contains("Board State: blocked-only-board"));
    assert!(format_metrics(&metrics).contains(
        "#737 Blocked only: missing unblock task Next: record an explicit owner/command"
    ));
}

#[test]
fn blocked_task_summaries_include_dependency_recovery_hints() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "001-parent.md",
        "id: 1\ntitle: Parent\nstatus: done\npriority: high\n",
    );
    write_board_task(
        tmp.path(),
        "002-child.md",
        "id: 2\ntitle: Child\nstatus: blocked\npriority: high\nblocked_on: waiting on parent\ndepends_on:\n  - 1\nclaimed_by: eng-1\n",
    );
    write_board_task(
        tmp.path(),
        "003-missing.md",
        "id: 3\ntitle: Missing dep\nstatus: blocked\npriority: high\nblocked_on: waiting on missing\ndepends_on:\n  - 404\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(
        metrics.blocked_task_summaries,
        vec![
            "#2 Child: waiting on parent Next: clear stale dependency blocker and requeue"
                .to_string(),
            "#3 Missing dep: waiting on missing Next: restore or remove missing dependency #404"
                .to_string(),
        ]
    );
}

#[test]
fn compute_metrics_distinguishes_review_backlog_gated_board() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "041-review.md",
        "id: 41\ntitle: Needs review\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.board_state, WorkflowBoardState::ReviewBacklogGated);
    assert_eq!(metrics.runnable_count, 0);
    assert_eq!(metrics.actionable_review_count, 1);
}

#[test]
fn compute_metrics_keeps_review_backlog_gated_with_review_drain_followup() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "736-review.md",
        "id: 736\ntitle: Review branch\nstatus: review\npriority: high\nclaimed_by: eng-1\ncommit: abc1234\n",
    );
    write_board_task(
        tmp.path(),
        "738-held.md",
        "id: 738\ntitle: Held release\nstatus: blocked\npriority: high\nblocked_on: waiting on #736 review disposition\n",
    );
    write_board_task(
        tmp.path(),
        "739-review-drain.md",
        "id: 739\ntitle: Drain #736 review backlog\nstatus: todo\npriority: high\ndepends_on:\n  - 736\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.board_state, WorkflowBoardState::ReviewBacklogGated);
    assert_eq!(metrics.runnable_count, 0);
    assert_eq!(metrics.actionable_review_count, 1);
    assert_eq!(metrics.blocked_count, 1);
    assert!(
        format_metrics(&metrics).contains("Implementation Work: review backlog is the bottleneck")
    );
}

#[test]
fn compute_metrics_prefers_runnable_state_for_mixed_blocked_and_runnable_board() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "001-runnable.md",
        "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
    );
    write_board_task(
        tmp.path(),
        "002-blocked.md",
        "id: 2\ntitle: Blocked\nstatus: blocked\npriority: high\nblocked_on: external dependency\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.board_state, WorkflowBoardState::RunnableBoard);
    assert_eq!(metrics.runnable_count, 1);
    assert_eq!(metrics.blocked_count, 1);
}

#[test]
fn compute_metrics_excludes_blocked_review_from_actionable_review_count() {
    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "001-actionable-review.md",
        "id: 1\ntitle: Actionable review\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
    );
    write_board_task(
        tmp.path(),
        "002-blocked-review.md",
        "id: 2\ntitle: Blocked review\nstatus: review\npriority: high\nclaimed_by: eng-1\nblocked_on: external reviewer\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.in_review_count, 2);
    assert_eq!(metrics.actionable_review_count, 1);
}

#[test]
fn workflow_metrics_section_returns_none_when_mode_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    fs::create_dir_all(&team_config_dir).unwrap();
    fs::write(team_config_dir.join("team.yaml"), "team: test\n").unwrap();

    assert!(workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).is_none());
}

#[test]
fn workflow_metrics_section_returns_formatted_metrics_when_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    fs::create_dir_all(&team_config_dir).unwrap();
    fs::write(
        team_config_dir.join("team.yaml"),
        "team: test\nworkflow_mode: hybrid\n",
    )
    .unwrap();
    write_board_task(
        tmp.path(),
        "001-runnable.md",
        "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
    );

    let (formatted, metrics) = workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

    assert!(formatted.contains("Workflow Metrics"));
    assert!(formatted.contains("Runnable: 1"));
    assert!(formatted.contains("Blocked Dispatch: -"));
    assert_eq!(metrics.runnable_count, 1);
}

#[test]
fn workflow_metrics_explains_todo_dependency_blocker() {
    let tmp = tempfile::tempdir().unwrap();
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    fs::create_dir_all(&team_config_dir).unwrap();
    fs::write(
        team_config_dir.join("team.yaml"),
        "team: test\nworkflow_mode: hybrid\n",
    )
    .unwrap();
    write_board_task(
        tmp.path(),
        "001-parent.md",
        "id: 1\ntitle: Parent\nstatus: blocked\npriority: high\n",
    );
    write_board_task(
        tmp.path(),
        "002-child.md",
        "id: 2\ntitle: Child\nstatus: todo\npriority: high\ndepends_on:\n  - 1\n",
    );

    let (formatted, metrics) = workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.runnable_count, 0);
    assert_eq!(
        metrics.blocked_dispatch_reasons,
        vec!["#2 Child: unmet dependency #1 (blocked)".to_string()]
    );
    assert!(formatted.contains("Blocked Dispatch: #2 Child: unmet dependency #1 (blocked)"));
}

#[test]
fn workflow_metrics_section_returns_formatted_metrics_for_board_first() {
    let tmp = tempfile::tempdir().unwrap();
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    fs::create_dir_all(&team_config_dir).unwrap();
    fs::write(
        team_config_dir.join("team.yaml"),
        "team: test\nworkflow_mode: board_first\n",
    )
    .unwrap();
    write_board_task(
        tmp.path(),
        "001-runnable.md",
        "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
    );

    let (formatted, metrics) = workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

    assert!(formatted.contains("Workflow Metrics"));
    assert!(formatted.contains("Runnable: 1"));
    assert_eq!(metrics.runnable_count, 1);
}

#[test]
fn workflow_metrics_section_repairs_legacy_telemetry_db_before_querying() {
    let tmp = tempfile::tempdir().unwrap();
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    fs::create_dir_all(&team_config_dir).unwrap();
    fs::write(
        team_config_dir.join("team.yaml"),
        "team: test\nworkflow_mode: hybrid\n",
    )
    .unwrap();
    write_board_task(
        tmp.path(),
        "001-runnable.md",
        "id: 1\ntitle: Runnable\nstatus: todo\npriority: high\n",
    );

    let legacy = create_legacy_telemetry_db(tmp.path());
    let completed = TeamEvent::task_completed("eng-1", Some("1"));
    let merged = TeamEvent::task_auto_merged_with_mode(
        "eng-1",
        "1",
        0.9,
        2,
        30,
        Some(crate::team::merge::MergeMode::DirectRoot),
    );
    for event in [completed, merged] {
        legacy
            .execute(
                "INSERT INTO events (timestamp, event_type, role, task_id, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    event.ts as i64,
                    event.event,
                    event.role,
                    event.task,
                    serde_json::to_string(&event).unwrap()
                ],
            )
            .unwrap();
    }
    drop(legacy);

    let (formatted, metrics) = workflow_metrics_section(tmp.path(), &[engineer("eng-1")]).unwrap();

    assert!(formatted.contains("Workflow Metrics"));
    assert!(formatted.contains("Auto-merge Rate: 100%"));
    assert_eq!(metrics.auto_merge_count, 1);

    let conn = crate::team::telemetry_db::open(tmp.path()).unwrap();
    let repairs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE event_type = 'telemetry_schema_repaired'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(repairs, 1);
}

#[test]
fn build_team_status_json_report_serializes_machine_readable_json() {
    let report = build_team_status_json_report(TeamStatusJsonReportInput {
        team: "test".to_string(),
        session: "batty-test".to_string(),
        session_running: true,
        paused: false,
        main_smoke: None,
        watchdog: WatchdogStatus {
            state: "running".to_string(),
            restart_count: 2,
            current_backoff_secs: None,
            last_exit_category: Some("unknown".to_string()),
            last_exit_reason: Some("daemon exited with status 101".to_string()),
            ..WatchdogStatus::default()
        },
        workflow_metrics: Some(WorkflowMetrics {
            runnable_count: 1,
            ..WorkflowMetrics::default()
        }),
        publish_handoff: Some(crate::release::ReleasePublishHandoff {
            generated_at: "2026-04-24T00:00:00Z".to_string(),
            path: ".batty/reports/release/publish-handoff.json".to_string(),
            markdown_path: ".batty/releases/publish-handoff.md".to_string(),
            status: "blocked".to_string(),
            package_name: Some("batty".to_string()),
            version: Some("0.10.0".to_string()),
            tag: Some("v0.10.0".to_string()),
            git_ref: Some("abc123".to_string()),
            branch: Some("main".to_string()),
            release_notes_path: Some(".batty/releases/v0.10.0.md".to_string()),
            changelog_path: "CHANGELOG.md".to_string(),
            release_record_success: true,
            release_record_reason: "created annotated tag".to_string(),
            verification: crate::release::ReleasePublishVerificationEvidence {
                command: Some("cargo test".to_string()),
                summary: Some("cargo test passed".to_string()),
                passed: true,
            },
            manual_publish_commands: vec![
                "git push origin main".to_string(),
                "git push origin v0.10.0".to_string(),
                "cargo publish --package batty".to_string(),
            ],
            blocked_reasons: vec!["missing_publish_credentials: token missing".to_string()],
        }),
        active_tasks: Vec::new(),
        review_queue: Vec::new(),
        optional_subsystems: None,
        engineer_profiles: Some(vec![
            crate::team::telemetry_db::EngineerPerformanceProfileRow {
                role: "eng-1".to_string(),
                completed_tasks: 2,
                avg_task_completion_secs: Some(1800.0),
                lines_per_hour: Some(120.0),
                first_pass_test_rate: Some(0.5),
                context_exhaustion_frequency: Some(0.0),
            },
        ]),
        members: vec![TeamStatusRow {
            name: "eng-1".to_string(),
            role: "engineer".to_string(),
            role_type: "Engineer".to_string(),
            agent: Some("codex".to_string()),
            reports_to: Some("manager".to_string()),
            state: "idle".to_string(),
            pending_inbox: 0,
            triage_backlog: 0,
            active_owned_tasks: Vec::new(),
            review_owned_tasks: Vec::new(),
            signal: None,
            runtime_label: Some("idle".to_string()),
            worktree_staleness: None,
            health: AgentHealthSummary::default(),
            health_summary: "-".to_string(),
            eta: "-".to_string(),
        }],
    });

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["team"], "test");
    assert_eq!(json["running"], true);
    assert_eq!(json["watchdog"]["restart_count"], 2);
    assert_eq!(json["health"]["member_count"], 1);
    assert_eq!(json["workflow_metrics"]["runnable_count"], 1);
    assert_eq!(
        json["publish_handoff"]["path"],
        ".batty/reports/release/publish-handoff.json"
    );
    assert_eq!(json["publish_handoff"]["status"], "blocked");
    assert!(json["members"].is_array());
    assert_eq!(json["engineer_profiles"][0]["role"], "eng-1");
}

#[test]
fn format_publish_handoff_summary_reports_path_and_blocked_state() {
    let handoff = crate::release::ReleasePublishHandoff {
        generated_at: "2026-04-24T00:00:00Z".to_string(),
        path: ".batty/reports/release/publish-handoff.json".to_string(),
        markdown_path: ".batty/releases/publish-handoff.md".to_string(),
        status: "blocked".to_string(),
        package_name: Some("batty".to_string()),
        version: Some("0.10.0".to_string()),
        tag: Some("v0.10.0".to_string()),
        git_ref: Some("abc123".to_string()),
        branch: Some("main".to_string()),
        release_notes_path: Some(".batty/releases/v0.10.0.md".to_string()),
        changelog_path: "CHANGELOG.md".to_string(),
        release_record_success: false,
        release_record_reason: "dirty_main".to_string(),
        verification: crate::release::ReleasePublishVerificationEvidence {
            command: Some("cargo test".to_string()),
            summary: Some("cargo test passed".to_string()),
            passed: false,
        },
        manual_publish_commands: vec!["git push origin main".to_string()],
        blocked_reasons: vec!["dirty_main: main worktree has uncommitted changes".to_string()],
    };

    let rendered = format_publish_handoff_summary(&handoff);

    assert!(rendered.contains(".batty/reports/release/publish-handoff.json"));
    assert!(rendered.contains("(blocked)"));
    assert!(rendered.contains("dirty_main"));
}

#[test]
fn parse_assigned_task_id_accepts_plain_numeric_values() {
    assert_eq!(parse_assigned_task_id("42"), Some(42));
}

#[test]
fn parse_assigned_task_id_extracts_task_hash_values() {
    assert_eq!(
        parse_assigned_task_id("Task #119: expand coverage"),
        Some(119)
    );
    assert_eq!(parse_assigned_task_id("working on #508 next"), Some(508));
}

#[test]
fn parse_assigned_task_id_rejects_values_without_leading_digits() {
    assert_eq!(parse_assigned_task_id("Task #abc"), None);
    assert_eq!(parse_assigned_task_id("no task here"), None);
}

#[test]
fn format_health_duration_formats_seconds() {
    assert_eq!(format_health_duration(59), "59s");
}

#[test]
fn format_health_duration_formats_minutes() {
    assert_eq!(format_health_duration(60), "1m");
}

#[test]
fn format_health_duration_formats_hours() {
    assert_eq!(format_health_duration(3_600), "1h");
}

#[test]
fn format_health_duration_formats_days() {
    assert_eq!(format_health_duration(86_400), "1d");
}

#[test]
fn merge_status_signal_combines_existing_triage_and_review_signals() {
    let mut pressure = SupervisoryPressureSnapshot::default();
    pressure.add_pressure(
        crate::team::supervisory_notice::SupervisoryPressure::TriageBacklog,
        2,
    );
    pressure.add_pressure(
        crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
        1,
    );
    assert_eq!(
        merge_status_signal(
            Some("nudged".to_string()),
            None,
            None,
            Some("manager (manager) stalled after 5m: no actionable progress".to_string()),
            &pressure,
            0,
        ),
        Some("nudged, pressure 2: review backlog (1)".to_string())
    );
}

#[test]
fn merge_status_signal_returns_none_when_no_signals_exist() {
    assert_eq!(
        merge_status_signal(
            None,
            None,
            None,
            None,
            &SupervisoryPressureSnapshot::default(),
            0,
        ),
        None
    );
}

#[test]
fn agent_health_by_member_defaults_without_events_or_state() {
    let tmp = tempfile::tempdir().unwrap();
    let health = agent_health_by_member(tmp.path(), &[engineer("eng-1"), engineer("eng-2")]);

    assert_eq!(health.get("eng-1"), Some(&AgentHealthSummary::default()));
    assert_eq!(health.get("eng-2"), Some(&AgentHealthSummary::default()));
}

#[test]
fn board_status_task_queues_split_active_and_review_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/task-41\nclass: standard\n---\n",
    )
    .unwrap();
    fs::write(
        tasks_dir.join("042-review.md"),
        "---\nid: 42\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-2\nreview_owner: manager\nnext_action: review now\nclass: standard\n---\n",
    )
    .unwrap();
    fs::write(
        tasks_dir.join("043-done.md"),
        "---\nid: 43\ntitle: Done task\nstatus: done\npriority: low\nclass: standard\n---\n",
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert_eq!(active_tasks.len(), 1);
    assert_eq!(active_tasks[0].id, 41);
    assert_eq!(active_tasks[0].branch.as_deref(), Some("eng-1/task-41"));
    assert_eq!(review_queue.len(), 1);
    assert_eq!(review_queue[0].id, 42);
    assert_eq!(review_queue[0].review_owner.as_deref(), Some("manager"));
    assert_eq!(review_queue[0].next_action.as_deref(), Some("review now"));
    assert!(review_queue[0].test_summary.is_none());
}

#[test]
fn format_review_blocks_surfaces_dirty_main_next_action() {
    let formatted = format_review_blocks(&[StatusTaskEntry {
        id: 42,
        title: "Review task".to_string(),
        status: "review".to_string(),
        priority: "high".to_string(),
        claimed_by: Some("eng-1".to_string()),
        review_owner: Some("manager".to_string()),
        blocked_on: Some(
            "Dirty source paths: src/lib.rs. Next action: commit, stash, or clean these root worktree changes before retrying the review merge."
                .to_string(),
        ),
        branch: Some("eng-1/42".to_string()),
        worktree_path: None,
        commit: None,
        branch_mismatch: None,
        next_action: None,
        test_summary: None,
        latest_artifact: None,
        failed_test_state: None,
    }])
    .unwrap();

    assert!(formatted.contains("Review Blocks"));
    assert!(formatted.contains("#42 Review task"));
    assert!(formatted.contains("src/lib.rs"));
    assert!(formatted.contains("retry review"));
}

#[test]
fn board_status_task_queues_marks_stale_review_when_engineer_moved_on() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-stale-review");
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("042-review.md"),
        "---\nid: 42\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-1\nreview_owner: manager\nclass: standard\n---\n",
    )
    .unwrap();
    fs::write(
        tasks_dir.join("043-active.md"),
        "---\nid: 43\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/43\nclass: standard\n---\n",
    )
    .unwrap();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/43").unwrap();
    std::fs::write(worktree_dir.join("active-43.txt"), "active branch\n").unwrap();
    crate::team::test_support::git_ok(&worktree_dir, &["add", "active-43.txt"]);
    crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "active branch"]);

    let (_, review_queue) = board_status_task_queues(&repo).unwrap();

    assert_eq!(review_queue.len(), 1);
    assert_eq!(
        review_queue[0].next_action.as_deref(),
        Some("stale review -> merge: eng-1 already moved to task #43 on branch `eng-1/43`")
    );
}

#[test]
fn board_status_task_queues_marks_branch_mismatch_review_as_rework() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("042-review.md"),
        "---\nid: 42\ntitle: Review task\nstatus: review\npriority: medium\nclaimed_by: eng-1\nreview_owner: manager\nbranch: eng-1/task-99\nclass: standard\n---\n",
    )
    .unwrap();

    let (_, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert_eq!(review_queue.len(), 1);
    assert_eq!(
        review_queue[0].next_action.as_deref(),
        Some(
            "stale review -> rework: branch `eng-1/task-99` references task(s) #99 but assigned task is #42"
        )
    );
}

#[test]
fn board_status_task_queues_infers_worktree_metadata_when_frontmatter_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-infer");
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
    )
    .unwrap();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        "eng-1",
        &team_config_dir,
    )
    .unwrap();
    fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    crate::team::test_support::git_ok(&worktree_dir, &["add", "note.txt"]);
    crate::team::test_support::git_ok(&worktree_dir, &["commit", "-m", "note"]);

    let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(active_tasks.len(), 1);
    assert_eq!(
        active_tasks[0].worktree_path.as_deref(),
        Some(".batty/worktrees/eng-1")
    );
    assert!(
        active_tasks[0]
            .branch
            .as_deref()
            .is_some_and(|branch| branch.contains("eng-1"))
    );
    assert!(active_tasks[0].commit.as_deref().is_some());
    assert_eq!(
        active_tasks[0].branch_mismatch.as_deref(),
        Some("branch mismatch (#41 on eng-1; expected eng-1/41)")
    );
    assert!(active_tasks[0].test_summary.is_none());
}

#[test]
fn board_status_task_queues_repairs_hidden_in_progress_task_frontmatter() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-repair-hidden");
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let task_path = tasks_dir.join("041-hidden-active.md");
    fs::write(
        &task_path,
        "---\nid: 41\ntitle: Hidden active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nblocked: waiting on reviewer\nclass: standard\n---\n",
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(active_tasks.len(), 1);
    assert_eq!(active_tasks[0].id, 41);
    assert_eq!(active_tasks[0].status, "in-progress");
    assert_eq!(
        active_tasks[0].blocked_on.as_deref(),
        Some("waiting on reviewer")
    );

    let content = fs::read_to_string(&task_path).unwrap();
    assert!(content.contains("blocked: true"));
    assert!(content.contains("block_reason: waiting on reviewer"));
    assert!(content.contains("blocked_on: waiting on reviewer"));
}

#[test]
fn board_status_task_queues_repairs_legacy_timestamp_frontmatter() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-repair-timestamp");
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let task_path = tasks_dir.join("623-stale-review.md");
    fs::write(
        &task_path,
        "---\nid: 623\ntitle: stale review\nstatus: review\npriority: high\nclaimed_by: eng-1\ncreated: 2026-04-10T16:31:02.743151-04:00\nupdated: 2026-04-10T19:26:40-0400\nreview_owner: manager\nclass: standard\n---\n\nTask body.\n",
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

    assert!(active_tasks.is_empty());
    assert_eq!(review_queue.len(), 1);
    assert_eq!(review_queue[0].id, 623);
    let content = fs::read_to_string(&task_path).unwrap();
    assert!(content.contains("updated: 2026-04-10T19:26:40-04:00"));
    assert!(content.ends_with("\n\nTask body.\n"));
}

#[test]
fn board_status_task_queues_surfaces_detached_head_branch_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-detached-board");
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
    )
    .unwrap();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/41").unwrap();
    let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    crate::team::test_support::git_ok(&worktree_dir, &["checkout", "--detach", &head]);

    let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(active_tasks[0].branch.as_deref(), Some("HEAD"));
    assert_eq!(
        active_tasks[0].branch_mismatch.as_deref(),
        Some(
            "branch recovery blocked (#41 detached HEAD; expected eng-1/41; manual checkout required)"
        )
    );
}

#[test]
fn board_status_task_queues_surfaces_dirty_branch_recovery_blocker() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = crate::team::test_support::init_git_repo(&tmp, "status-dirty-board");
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\n---\n",
    )
    .unwrap();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = crate::team::task_loop::engineer_base_branch_name("eng-1");
    crate::team::task_loop::setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &base_branch,
        &team_config_dir,
    )
    .unwrap();
    fs::write(worktree_dir.join("scratch.txt"), "dirty\n").unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(&repo).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(
        active_tasks[0].branch_mismatch.as_deref(),
        Some("branch recovery blocked (#41 on eng-main/eng-1; expected eng-1/41; dirty worktree)")
    );
}

#[test]
fn board_status_task_queues_surfaces_failed_test_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\nartifacts:\n  - artifacts/retry-41.log\noutcome: verification_retry_required\ntest_results:\n  framework: cargo\n  total: 3\n  passed: 2\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n      message: assertion failed\n      location: src/parser.rs:12:5\n---\n",
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(active_tasks.len(), 1);
    assert_eq!(
        active_tasks[0].test_summary.as_deref(),
        Some("1 tests failed: parser::it_works (assertion failed at src/parser.rs:12:5)")
    );
    assert_eq!(
        active_tasks[0].latest_artifact.as_deref(),
        Some("artifacts/retry-41.log")
    );
    assert_eq!(
        active_tasks[0].failed_test_state.as_deref(),
        Some("dispatchable rework")
    );
    assert_eq!(
        format_failed_test_task(&active_tasks[0]).as_deref(),
        Some(
            "#41 Active task (owner: eng-1; artifact: artifacts/retry-41.log; dispatchable rework): 1 tests failed: parser::it_works (assertion failed at src/parser.rs:12:5)"
        )
    );
}

#[test]
fn board_status_task_queues_marks_active_owner_retry_owner_locked() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\nartifacts:\n  - artifacts/retry-41.log\noutcome: verification_retry_required\ntest_results:\n  framework: cargo\n  total: 3\n  passed: 2\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n---\n",
    )
    .unwrap();
    let daemon_state = serde_json::json!({
        "states": {"eng-1": "working"},
        "active_tasks": {"eng-1": 41}
    });
    fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    fs::write(
        daemon_state_path(tmp.path()),
        serde_json::to_vec_pretty(&daemon_state).unwrap(),
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(
        active_tasks[0].failed_test_state.as_deref(),
        Some("owner-locked")
    );
    assert_eq!(
        format_failed_test_task(&active_tasks[0]).as_deref(),
        Some(
            "#41 Active task (owner: eng-1; artifact: artifacts/retry-41.log; owner-locked): 1 tests failed: parser::it_works"
        )
    );
}

#[test]
fn board_status_task_queues_surfaces_retry_artifact_without_test_results() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("728-active.md"),
        "---\nid: 728\ntitle: Verification retry\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\nartifacts:\n  - artifacts/retry-728.log\noutcome: verification_retry_required\n---\n",
    )
    .unwrap();
    let daemon_state = serde_json::json!({
        "states": {"eng-1": "working", "eng-2": "idle"},
        "active_tasks": {"eng-1": 777}
    });
    fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    fs::write(
        daemon_state_path(tmp.path()),
        serde_json::to_vec_pretty(&daemon_state).unwrap(),
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(
        active_tasks[0].test_summary.as_deref(),
        Some("verification retry required")
    );
    assert_eq!(
        active_tasks[0].latest_artifact.as_deref(),
        Some("artifacts/retry-728.log")
    );
    assert_eq!(
        active_tasks[0].failed_test_state.as_deref(),
        Some("dispatchable rework")
    );
    assert_eq!(
        format_failed_test_task(&active_tasks[0]).as_deref(),
        Some(
            "#728 Verification retry (owner: eng-1; artifact: artifacts/retry-728.log; dispatchable rework): verification retry required"
        )
    );
}

#[test]
fn board_status_task_queues_labels_failed_tests_owner_locked_without_retry_artifact() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("041-active.md"),
        "---\nid: 41\ntitle: Active task\nstatus: in-progress\npriority: high\nclaimed_by: eng-1\nclass: standard\ntests_run: true\ntests_passed: false\ntest_results:\n  framework: cargo\n  total: 1\n  passed: 0\n  failed: 1\n  ignored: 0\n  failures:\n    - test_name: parser::it_works\n---\n",
    )
    .unwrap();

    let (active_tasks, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert!(review_queue.is_empty());
    assert_eq!(
        active_tasks[0].failed_test_state.as_deref(),
        Some("owner-locked")
    );
    assert_eq!(
        format_failed_test_task(&active_tasks[0]).as_deref(),
        Some(
            "#41 Active task (owner: eng-1; artifact: none; owner-locked): 1 tests failed: parser::it_works"
        )
    );
}

#[test]
fn session_failed_tests_formatter_includes_owner_artifact_and_state() {
    let task = StatusTaskEntry {
        id: 728,
        title: "Verification retry".to_string(),
        status: "in-progress".to_string(),
        priority: "high".to_string(),
        claimed_by: Some("eng-1".to_string()),
        review_owner: None,
        blocked_on: None,
        branch: Some("eng-1/728".to_string()),
        worktree_path: None,
        commit: None,
        branch_mismatch: None,
        next_action: None,
        test_summary: Some("cargo test failed".to_string()),
        latest_artifact: Some("artifacts/728.log".to_string()),
        failed_test_state: Some("dispatchable rework".to_string()),
    };

    assert_eq!(
        format_failed_test_task(&task).as_deref(),
        Some(
            "#728 Verification retry (owner: eng-1; artifact: artifacts/728.log; dispatchable rework): cargo test failed"
        )
    );
}

#[test]
fn board_status_task_queues_surfaces_failed_github_feedback() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("042-review.md"),
        "---\nid: 42\ntitle: Review task\nstatus: review\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/42\ncommit: abcdef1\nclass: standard\n---\n",
    )
    .unwrap();
    crate::team::github_feedback::write_github_feedback_record(
        tmp.path(),
        &crate::team::github_feedback::GithubVerificationRecord {
            task_id: 42,
            branch: Some("eng-1/42".to_string()),
            commit: Some("abcdef1".to_string()),
            check_name: "ci/test".to_string(),
            status: "failure".to_string(),
            next_action: Some("fix failing CI".to_string()),
            details: Some("unit test failed".to_string()),
            ts: Some(1),
        },
    )
    .unwrap();

    let (_, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert_eq!(review_queue.len(), 1);
    assert_eq!(
        review_queue[0].blocked_on.as_deref(),
        Some("GitHub check failed: ci/test on eng-1/42@abcdef1 (unit test failed)")
    );
    assert_eq!(
        review_queue[0].next_action.as_deref(),
        Some("fix failing CI")
    );
    assert_eq!(
        review_queue[0].test_summary.as_deref(),
        Some("GitHub check failed: ci/test on eng-1/42@abcdef1")
    );
}

#[test]
fn board_status_task_queues_clears_github_failure_after_passing_record() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    fs::write(
        tasks_dir.join("042-review.md"),
        "---\nid: 42\ntitle: Review task\nstatus: review\npriority: high\nclaimed_by: eng-1\nbranch: eng-1/42\ncommit: abcdef1\nnext_action: review now\nclass: standard\n---\n",
    )
    .unwrap();
    for status in ["failure", "success"] {
        crate::team::github_feedback::write_github_feedback_record(
            tmp.path(),
            &crate::team::github_feedback::GithubVerificationRecord {
                task_id: 42,
                branch: Some("eng-1/42".to_string()),
                commit: Some("abcdef1".to_string()),
                check_name: "ci/test".to_string(),
                status: status.to_string(),
                next_action: Some("fix failing CI".to_string()),
                details: None,
                ts: Some(1),
            },
        )
        .unwrap();
    }

    let (_, review_queue) = board_status_task_queues(tmp.path()).unwrap();

    assert_eq!(review_queue.len(), 1);
    assert!(review_queue[0].blocked_on.is_none());
    assert_eq!(review_queue[0].next_action.as_deref(), Some("review now"));
    assert_eq!(
        review_queue[0].test_summary.as_deref(),
        Some("GitHub check passed: ci/test on eng-1/42@abcdef1")
    );
}

#[test]
fn build_team_status_json_report_includes_health_and_queues() {
    let report = build_team_status_json_report(TeamStatusJsonReportInput {
        team: "test".to_string(),
        session: "batty-test".to_string(),
        session_running: true,
        paused: true,
        main_smoke: None,
        watchdog: WatchdogStatus {
            state: "restarting".to_string(),
            restart_count: 1,
            current_backoff_secs: Some(4),
            last_exit_category: Some("unknown".to_string()),
            last_exit_reason: Some("daemon exited with status 101".to_string()),
            ..WatchdogStatus::default()
        },
        workflow_metrics: Some(WorkflowMetrics {
            runnable_count: 2,
            blocked_count: 1,
            in_review_count: 1,
            in_progress_count: 3,
            idle_with_runnable: vec!["eng-2".to_string()],
            top_runnable_tasks: vec!["#7 (high) Unstick manager inbox".to_string()],
            oldest_review_age_secs: Some(60),
            oldest_assignment_age_secs: Some(120),
            ..Default::default()
        }),
        publish_handoff: None,
        active_tasks: vec![StatusTaskEntry {
            id: 41,
            title: "Active task".to_string(),
            status: "in-progress".to_string(),
            priority: "high".to_string(),
            claimed_by: Some("eng-1".to_string()),
            review_owner: None,
            blocked_on: None,
            branch: Some("eng-1/task-41".to_string()),
            worktree_path: None,
            commit: None,
            branch_mismatch: None,
            next_action: None,
            test_summary: Some("1 tests failed: parser::it_works".to_string()),
            latest_artifact: Some("artifacts/failed-tests.log".to_string()),
            failed_test_state: Some("dispatchable rework".to_string()),
        }],
        review_queue: vec![StatusTaskEntry {
            id: 42,
            title: "Review task".to_string(),
            status: "review".to_string(),
            priority: "medium".to_string(),
            claimed_by: Some("eng-2".to_string()),
            review_owner: Some("manager".to_string()),
            blocked_on: None,
            branch: None,
            worktree_path: None,
            commit: None,
            branch_mismatch: None,
            next_action: Some("review now".to_string()),
            test_summary: None,
            latest_artifact: None,
            failed_test_state: None,
        }],
        optional_subsystems: None,
        engineer_profiles: None,
        members: vec![
            TeamStatusRow {
                name: "eng-1".to_string(),
                role: "engineer".to_string(),
                role_type: "Engineer".to_string(),
                agent: Some("codex".to_string()),
                reports_to: Some("manager".to_string()),
                state: "working".to_string(),
                pending_inbox: 2,
                triage_backlog: 0,
                active_owned_tasks: vec![41],
                review_owned_tasks: vec![],
                signal: None,
                runtime_label: Some("working".to_string()),
                worktree_staleness: Some(3),
                health: AgentHealthSummary {
                    restart_count: 1,
                    context_exhaustion_count: 0,
                    proactive_handoff_count: 0,
                    delivery_failure_count: 0,
                    supervisory_digest_count: 0,
                    dispatch_fallback_count: 0,
                    dispatch_fallback_reason: None,
                    stale_active_cleared_count: 0,
                    stale_active_summary: None,
                    task_elapsed_secs: Some(30),
                    stall_reason: None,
                    stall_summary: None,
                    backend_health: crate::agent::BackendHealth::default(),
                },
                health_summary: "r1 t30s".to_string(),
                eta: "-".to_string(),
            },
            TeamStatusRow {
                name: "eng-2".to_string(),
                role: "engineer".to_string(),
                role_type: "Engineer".to_string(),
                agent: Some("codex".to_string()),
                reports_to: Some("manager".to_string()),
                state: "idle".to_string(),
                pending_inbox: 1,
                triage_backlog: 2,
                active_owned_tasks: vec![],
                review_owned_tasks: vec![42],
                signal: Some("needs review (1)".to_string()),
                runtime_label: Some("idle".to_string()),
                worktree_staleness: Some(0),
                health: AgentHealthSummary::default(),
                health_summary: "-".to_string(),
                eta: "-".to_string(),
            },
        ],
    });

    assert_eq!(report.team, "test");
    assert_eq!(report.watchdog.restart_count, 1);
    assert_eq!(report.active_tasks.len(), 1);
    assert_eq!(report.review_queue.len(), 1);
    assert!(report.paused);
    assert_eq!(report.health.member_count, 2);
    assert_eq!(report.health.active_member_count, 1);
    assert_eq!(report.health.pending_inbox_count, 3);
    assert_eq!(report.health.triage_backlog_count, 2);
    assert_eq!(report.health.unhealthy_members, vec!["eng-1".to_string()]);
    assert_eq!(report.workflow_metrics.unwrap().runnable_count, 2);
}

#[test]
fn format_engineer_profiles_renders_compact_table() {
    let rendered =
        format_engineer_profiles(&[crate::team::telemetry_db::EngineerPerformanceProfileRow {
            role: "eng-1".to_string(),
            completed_tasks: 3,
            avg_task_completion_secs: Some(5400.0),
            lines_per_hour: Some(42.5),
            first_pass_test_rate: Some(2.0 / 3.0),
            context_exhaustion_frequency: Some(1.0 / 3.0),
        }]);

    assert!(rendered.contains("Engineer Profiles"));
    assert!(rendered.contains("eng-1"));
    assert!(rendered.contains("1h"));
    assert!(rendered.contains("42.5"));
    assert!(rendered.contains("67%"));
    assert!(rendered.contains("33%"));
}

#[test]
fn format_benched_engineers_includes_reason_and_timestamp() {
    let rendered = format_benched_engineers(&crate::team::bench::BenchState {
        benched: std::collections::BTreeMap::from([(
            "eng-1".to_string(),
            crate::team::bench::BenchEntry {
                timestamp: "2026-04-10T12:00:00Z".to_string(),
                reason: Some("session end".to_string()),
            },
        )]),
    })
    .unwrap();

    assert!(rendered.contains("Benched Engineers"));
    assert!(rendered.contains("eng-1"));
    assert!(rendered.contains("2026-04-10T12:00:00Z"));
    assert!(rendered.contains("session end"));
}

#[test]
fn load_watchdog_status_marks_circuit_breaker_and_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    fs::write(
        watchdog_pid_path(tmp.path()),
        std::process::id().to_string(),
    )
    .unwrap();
    std::fs::write(
        watchdog_state_path(tmp.path()),
        serde_json::json!({
            "restart_count": 5,
            "circuit_breaker_tripped": true,
            "last_exit_reason": "daemon exited with status 101"
        })
        .to_string(),
    )
    .unwrap();

    let watchdog = load_watchdog_status(tmp.path(), true);

    assert_eq!(watchdog.state, "circuit-open");
    assert_eq!(watchdog.restart_count, 5);
    assert_eq!(
        watchdog.last_exit_reason.as_deref(),
        Some("daemon exited with status 101")
    );
}

#[test]
fn load_watchdog_status_reports_offline_for_stale_dead_pids() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    let dead_watchdog_pid = exited_test_pid();
    let dead_daemon_pid = exited_test_pid();
    fs::write(watchdog_pid_path(tmp.path()), dead_watchdog_pid.to_string()).unwrap();
    fs::write(
        daemon_child_pid_path(tmp.path()),
        dead_daemon_pid.to_string(),
    )
    .unwrap();
    fs::write(
        watchdog_state_path(tmp.path()),
        serde_json::json!({
            "restart_count": 1,
            "child_pid": dead_daemon_pid
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        daemon_state_path(tmp.path()),
        serde_json::json!({"saved_at": 1, "clean_shutdown": false}).to_string(),
    )
    .unwrap();
    fs::write(daemon_log_path(tmp.path()), "daemon stopped\n").unwrap();

    let watchdog = load_watchdog_status(tmp.path(), true);

    assert_eq!(watchdog.state, "offline");
    assert_eq!(watchdog.watchdog_pid, Some(dead_watchdog_pid));
    assert!(!watchdog.watchdog_pid_live);
    assert_eq!(watchdog.daemon_pid, Some(dead_daemon_pid));
    assert!(!watchdog.daemon_pid_live);
    assert!(watchdog.daemon_state_updated_at.is_some());
    assert!(watchdog.daemon_state_age_secs.is_some());
    assert!(watchdog.daemon_log_updated_at.is_some());
    assert!(watchdog.daemon_log_age_secs.is_some());

    let summary = format_watchdog_summary(&watchdog);
    assert!(summary.contains("offline"));
    assert!(summary.contains(&format!("watchdog-pid={dead_watchdog_pid}:dead")));
    assert!(summary.contains(&format!("daemon-pid={dead_daemon_pid}:dead")));
    assert!(summary.contains("state-age="));
    assert!(summary.contains("log-age="));
}

fn exited_test_pid() -> u32 {
    #[cfg(unix)]
    {
        let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }
    #[cfg(not(unix))]
    {
        999_998
    }
}

#[cfg(unix)]
#[test]
fn load_watchdog_status_reports_running_for_live_watchdog_and_daemon_pids() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    let mut watchdog_process = Command::new("sleep").arg("5").spawn().unwrap();
    let mut daemon_process = Command::new("sleep").arg("5").spawn().unwrap();
    fs::write(
        watchdog_pid_path(tmp.path()),
        watchdog_process.id().to_string(),
    )
    .unwrap();
    fs::write(
        daemon_child_pid_path(tmp.path()),
        daemon_process.id().to_string(),
    )
    .unwrap();
    fs::write(
        watchdog_state_path(tmp.path()),
        serde_json::json!({
            "restart_count": 0,
            "child_pid": daemon_process.id()
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        daemon_state_path(tmp.path()),
        serde_json::json!({"saved_at": crate::team::now_unix(), "clean_shutdown": false})
            .to_string(),
    )
    .unwrap();
    fs::write(daemon_log_path(tmp.path()), "daemon alive\n").unwrap();

    let watchdog = load_watchdog_status(tmp.path(), true);

    assert_eq!(watchdog.state, "running");
    assert_eq!(watchdog.watchdog_pid, Some(watchdog_process.id()));
    assert!(watchdog.watchdog_pid_live);
    assert_eq!(watchdog.daemon_pid, Some(daemon_process.id()));
    assert!(watchdog.daemon_pid_live);
    assert!(format_watchdog_summary(&watchdog).contains("daemon-pid="));

    let _ = watchdog_process.kill();
    let _ = daemon_process.kill();
    let _ = watchdog_process.wait();
    let _ = daemon_process.wait();
}

#[test]
fn format_watchdog_summary_includes_backoff_and_reason() {
    let summary = format_watchdog_summary(&WatchdogStatus {
        state: "restarting".to_string(),
        restart_count: 2,
        current_backoff_secs: Some(4),
        last_exit_category: Some("unknown".to_string()),
        last_exit_reason: Some("daemon exited with status 101".to_string()),
        ..WatchdogStatus::default()
    });

    assert!(summary.contains("restarting"));
    assert!(summary.contains("r2"));
    assert!(summary.contains("backoff=4s"));
    assert!(summary.contains("daemon exited with status 101"));
}

#[test]
fn format_watchdog_summary_surfaces_orphan_codex_exec_processes() {
    let summary = format_watchdog_summary(&WatchdogStatus {
        state: "offline".to_string(),
        restart_count: 1,
        orphan_codex_execs: vec![OrphanProcessStatus {
            pid: 42,
            ppid: 1,
            command: "codex exec --json -".to_string(),
        }],
        ..WatchdogStatus::default()
    });

    assert!(summary.contains("orphan-codex-exec=1 [42<-1]"));
}

#[test]
fn format_main_smoke_summary_includes_commit_and_suspects() {
    let summary = format_main_smoke_summary(&MainSmokeState {
        broken: true,
        pause_dispatch: true,
        last_run_at: 1,
        last_success_commit: Some("aaa1111".to_string()),
        broken_commit: Some("bbb2222".to_string()),
        suspects: vec![
            "bbb2222 break main".to_string(),
            "ccc3333 prior".to_string(),
        ],
        summary: Some("could not compile `batty-cli`".to_string()),
    });
    assert!(summary.contains("BROKEN by bbb2222"));
    assert!(summary.contains("bbb2222 break main"));
    assert!(summary.contains("could not compile `batty-cli`"));
}

#[test]
fn build_team_status_json_report_includes_main_smoke_state() {
    let report = build_team_status_json_report(TeamStatusJsonReportInput {
        team: "test".to_string(),
        session: "batty-test".to_string(),
        session_running: true,
        paused: false,
        main_smoke: Some(MainSmokeState {
            broken: true,
            pause_dispatch: true,
            last_run_at: 1,
            last_success_commit: None,
            broken_commit: Some("abc1234".to_string()),
            suspects: vec!["abc1234 break main".to_string()],
            summary: Some("could not compile `batty-cli`".to_string()),
        }),
        watchdog: WatchdogStatus {
            state: "running".to_string(),
            restart_count: 0,
            current_backoff_secs: None,
            last_exit_category: None,
            last_exit_reason: None,
            ..WatchdogStatus::default()
        },
        workflow_metrics: None,
        publish_handoff: None,
        active_tasks: Vec::new(),
        review_queue: Vec::new(),
        optional_subsystems: None,
        engineer_profiles: None,
        members: Vec::new(),
    });
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["main_smoke"]["broken"].as_bool(), Some(true));
    assert_eq!(
        json["main_smoke"]["broken_commit"].as_str(),
        Some("abc1234")
    );
}

#[test]
fn format_standup_status_marks_paused_while_member_is_working() {
    assert_eq!(
        format_standup_status(Some(Instant::now()), Duration::from_secs(600), true),
        " #[fg=244]standup paused#[default]"
    );
}

#[test]
fn format_nudge_status_marks_paused_while_member_is_working() {
    let schedule = NudgeSchedule {
        text: "check in".to_string(),
        interval: Duration::from_secs(600),
        idle_since: None,
        fired_this_idle: false,
        paused: true,
    };

    assert_eq!(
        format_nudge_status(Some(&schedule)),
        " #[fg=244]nudge paused#[default]"
    );
}

#[test]
fn compose_pane_status_label_shows_pending_inbox_count() {
    let label = compose_pane_status_label(PaneStatusLabelArgs {
        state: MemberState::Idle,
        pending_inbox: 3,
        triage_backlog: 2,
        active_task_ids: &[191],
        review_task_ids: &[193, 194],
        globally_paused: false,
        nudge_status: " #[fg=magenta]nudge 0:30#[default]",
        standup_status: "",
    });
    assert!(label.contains("idle"));
    assert!(label.contains("inbox 3"));
    assert!(label.contains("triage 2"));
    assert!(label.contains("task 191"));
    assert!(label.contains("review 2"));
    assert!(label.contains("nudge 0:30"));
}

#[test]
fn compose_pane_status_label_shows_zero_inbox_and_pause_state() {
    let label = compose_pane_status_label(PaneStatusLabelArgs {
        state: MemberState::Working,
        pending_inbox: 0,
        triage_backlog: 0,
        active_task_ids: &[],
        review_task_ids: &[],
        globally_paused: true,
        nudge_status: "",
        standup_status: "",
    });
    assert!(label.contains("working"));
    assert!(label.contains("inbox 0"));
    assert!(label.contains("PAUSED"));
}

#[test]
fn format_agent_health_summary_compacts_metrics() {
    let summary = format_agent_health_summary(&AgentHealthSummary {
        restart_count: 2,
        context_exhaustion_count: 1,
        proactive_handoff_count: 1,
        delivery_failure_count: 3,
        supervisory_digest_count: 1,
        dispatch_fallback_count: 0,
        dispatch_fallback_reason: None,
        stale_active_cleared_count: 1,
        stale_active_summary: Some("cleared stale active #42 (done)".to_string()),
        task_elapsed_secs: Some(750),
        stall_reason: None,
        stall_summary: None,
        backend_health: crate::agent::BackendHealth::default(),
    });

    assert_eq!(summary, "r2 c1 ph1 d3 sd1 sa1 t12m");
    assert_eq!(
        format_agent_health_summary(&AgentHealthSummary::default()),
        "-"
    );
}

#[test]
fn format_agent_health_summary_includes_supervisory_stall_token() {
    let summary = format_agent_health_summary(&AgentHealthSummary {
        stall_reason: Some("supervisory_stalled_manager_no_actionable_progress".to_string()),
        stall_summary: Some("lead (manager) stalled after 5m: no actionable progress".to_string()),
        ..AgentHealthSummary::default()
    });

    assert_eq!(summary, "stall:manager:working-timeout");
}

#[test]
fn format_agent_health_summary_uses_non_engineer_slo_tokens() {
    let cases = [
        (
            "supervisory_stalled_manager_review_backlog",
            RoleType::Manager,
            "stall:manager:review-wait-timeout",
        ),
        (
            "supervisory_stalled_manager_dispatch_gap",
            RoleType::Manager,
            "stall:manager:dispatch-gap-pressure",
        ),
        (
            "supervisory_stalled_architect_direct_report_packets",
            RoleType::Architect,
            "stall:architect:inbox-backlog-pressure",
        ),
        (
            "supervisory_stalled_architect_inbox_batching",
            RoleType::Architect,
            "stall:architect:inbox-backlog-pressure",
        ),
        (
            "supervisory_stalled_manager_working_timeout",
            RoleType::Manager,
            "stall:manager:working-timeout",
        ),
    ];

    for (reason, role_type, expected) in cases {
        let summary = format_agent_health_summary_for_role(
            &AgentHealthSummary {
                stall_reason: Some(reason.to_string()),
                stall_summary: Some("stalled".to_string()),
                ..AgentHealthSummary::default()
            },
            Some(role_type),
        );
        assert_eq!(summary, expected);
    }
}

#[test]
fn format_agent_health_summary_includes_dispatch_fallback_token() {
    let summary = format_agent_health_summary(&AgentHealthSummary {
        dispatch_fallback_count: 1,
        dispatch_fallback_reason: Some("manager_supervisory_no_actionable_progress".to_string()),
        ..AgentHealthSummary::default()
    });

    assert_eq!(summary, "fd1:manager_supervisory_no_actionable_progress");
}

#[test]
fn format_agent_health_summary_for_role_includes_supervisory_role_token() {
    let summary = format_agent_health_summary_for_role(
        &AgentHealthSummary {
            stall_reason: Some("supervisory_stalled_manager_shim_activity_only".to_string()),
            stall_summary: Some("lead (manager) stalled after 5m: shim activity only".to_string()),
            ..AgentHealthSummary::default()
        },
        Some(RoleType::Manager),
    );

    assert_eq!(summary, "stall:manager:working-timeout");
}

#[test]
fn agent_health_by_member_aggregates_events_and_active_task_elapsed() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();

    let mut assigned = TeamEvent::task_assigned("eng-1", "Task #42: fix it");
    assigned.ts = now_unix().saturating_sub(600);
    sink.emit(assigned).unwrap();

    let mut restarted = TeamEvent::agent_restarted("eng-1", "42", "context_exhausted", 2);
    restarted.ts = now_unix().saturating_sub(590);
    sink.emit(restarted).unwrap();

    let mut exhausted = TeamEvent::context_exhausted("eng-1", Some(42), Some(4_096));
    exhausted.ts = now_unix().saturating_sub(580);
    sink.emit(exhausted).unwrap();

    let mut proactive = TeamEvent::agent_restarted("eng-1", "42", "context_pressure", 1);
    proactive.ts = now_unix().saturating_sub(575);
    sink.emit(proactive).unwrap();

    let mut delivery_failed =
        TeamEvent::delivery_failed("eng-1", "manager", "message delivery failed after retries");
    delivery_failed.ts = now_unix().saturating_sub(570);
    sink.emit(delivery_failed).unwrap();

    let mut digest_emitted = TeamEvent::supervisory_digest_emitted("eng-1", 3, 1);
    digest_emitted.ts = now_unix().saturating_sub(565);
    sink.emit(digest_emitted).unwrap();

    let mut stale_active = TeamEvent::state_reconciliation(Some("eng-1"), Some("42"), "clear_done");
    stale_active.ts = now_unix().saturating_sub(563);
    sink.emit(stale_active).unwrap();

    let mut supervisory_stall = TeamEvent::stall_detected_with_reason(
        "eng-1",
        None,
        300,
        Some("supervisory_stalled_manager_no_actionable_progress"),
    );
    supervisory_stall.ts = now_unix().saturating_sub(560);
    supervisory_stall.task = Some("supervisory::eng-1".to_string());
    supervisory_stall.details =
        Some("eng-1 (manager) stalled after 5m: no actionable progress".to_string());
    sink.emit(supervisory_stall).unwrap();

    let daemon_state = serde_json::json!({
        "active_tasks": {"eng-1": 42},
        "retry_counts": {"eng-1": 1}
    });
    fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    fs::write(
        daemon_state_path(tmp.path()),
        serde_json::to_vec_pretty(&daemon_state).unwrap(),
    )
    .unwrap();

    let health = agent_health_by_member(tmp.path(), &[engineer("eng-1"), engineer("eng-2")]);
    let eng_1 = health.get("eng-1").unwrap();
    assert_eq!(eng_1.restart_count, 2);
    assert_eq!(eng_1.context_exhaustion_count, 1);
    assert_eq!(eng_1.proactive_handoff_count, 1);
    assert_eq!(eng_1.delivery_failure_count, 1);
    assert_eq!(eng_1.supervisory_digest_count, 1);
    assert_eq!(eng_1.stale_active_cleared_count, 1);
    assert_eq!(
        eng_1.stale_active_summary.as_deref(),
        Some("cleared stale active #42 (done)")
    );
    assert_eq!(
        eng_1.stall_reason.as_deref(),
        Some("supervisory_stalled_manager_no_actionable_progress")
    );
    assert_eq!(
        eng_1.stall_summary.as_deref(),
        Some("eng-1 (manager) stalled after 5m: no actionable progress")
    );
    assert!(eng_1.task_elapsed_secs.unwrap() >= 600);
    assert_eq!(health.get("eng-2").unwrap(), &AgentHealthSummary::default());
}

#[test]
fn format_agent_health_summary_includes_backend_health() {
    let summary = format_agent_health_summary(&AgentHealthSummary {
        backend_health: crate::agent::BackendHealth::Unreachable,
        ..AgentHealthSummary::default()
    });
    assert_eq!(summary, "B:unreachable");

    let summary = format_agent_health_summary(&AgentHealthSummary {
        backend_health: crate::agent::BackendHealth::Degraded,
        restart_count: 1,
        ..AgentHealthSummary::default()
    });
    assert_eq!(summary, "B:degraded r1");
}

#[test]
fn load_optional_subsystem_statuses_reads_budget_state() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();
    for _ in 0..6 {
        let mut event = TeamEvent::loop_step_error("process_telegram_queue", "telegram down");
        event.ts = now_unix();
        sink.emit(event).unwrap();
    }

    let daemon_state = serde_json::json!({
        "optional_subsystem_backoff": {"telegram": 2},
        "optional_subsystem_disabled_remaining_secs": {"telegram": 45}
    });
    fs::create_dir_all(tmp.path().join(".batty")).unwrap();
    fs::write(
        daemon_state_path(tmp.path()),
        serde_json::to_vec_pretty(&daemon_state).unwrap(),
    )
    .unwrap();

    let statuses = load_optional_subsystem_statuses(tmp.path());
    let telegram = statuses
        .iter()
        .find(|status| status.name == "telegram")
        .expect("telegram status should exist");
    assert_eq!(telegram.state, "disabled");
    assert_eq!(telegram.recent_errors, 6);
    assert_eq!(telegram.disabled_remaining_secs, Some(45));
    assert_eq!(telegram.backoff_stage, Some(2));
    assert_eq!(telegram.last_error.as_deref(), Some("telegram down"));
}

#[test]
fn agent_health_by_member_reads_health_changed_events() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();
    sink.emit(TeamEvent::health_changed("eng-1", "healthy→unreachable"))
        .unwrap();

    let health = agent_health_by_member(tmp.path(), &[engineer("eng-1")]);
    assert_eq!(
        health.get("eng-1").unwrap().backend_health,
        crate::agent::BackendHealth::Unreachable,
    );
}

#[test]
fn agent_health_by_member_uses_latest_health_event() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();
    sink.emit(TeamEvent::health_changed("eng-1", "healthy→unreachable"))
        .unwrap();
    sink.emit(TeamEvent::health_changed("eng-1", "unreachable→healthy"))
        .unwrap();

    let health = agent_health_by_member(tmp.path(), &[engineer("eng-1")]);
    assert_eq!(
        health.get("eng-1").unwrap().backend_health,
        crate::agent::BackendHealth::Healthy,
    );
}

#[test]
fn agent_health_by_member_clears_stall_from_previous_daemon_session() {
    // Regression: stall_detected events from prior daemon sessions
    // used to leak into the current status display, producing signals
    // like "manager (manager) stalled after 2h" on a freshly-restarted
    // member. The fix: clear supervisory_stall state when a newer
    // daemon_started event is seen.
    let tmp = tempfile::tempdir().unwrap();
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();

    // Old daemon session emits a stall for the manager.
    let mut old_stall = TeamEvent::stall_detected_with_reason(
        "manager",
        None,
        7200,
        Some("supervisory_inbox_batching"),
    );
    old_stall.task = Some("supervisory::manager".to_string());
    old_stall.details = Some("manager (manager) stalled after 2h: inbox batching".to_string());
    sink.emit(old_stall).unwrap();

    // Daemon restarts — this should clear the prior stall state.
    sink.emit(TeamEvent::daemon_started()).unwrap();

    // After the restart, no new stall events have been emitted for the
    // manager. The status display must NOT show the stale 2h stall.
    let health = agent_health_by_member(tmp.path(), &[manager("manager"), engineer("eng-1")]);
    let manager_health = health.get("manager").unwrap();
    assert!(
        !manager_health.has_supervisory_warning(),
        "manager should not carry a supervisory stall warning from a prior daemon session; got reason={:?} summary={:?}",
        manager_health.stall_reason,
        manager_health.stall_summary,
    );
}

#[test]
fn agent_health_by_member_keeps_stall_from_current_daemon_session() {
    // Companion to the clears-previous-session test: a stall event that
    // happens AFTER the latest daemon_started should be preserved.
    let tmp = tempfile::tempdir().unwrap();
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();

    sink.emit(TeamEvent::daemon_started()).unwrap();
    let mut stall = TeamEvent::stall_detected_with_reason(
        "manager",
        None,
        300,
        Some("supervisory_inbox_batching"),
    );
    stall.task = Some("supervisory::manager".to_string());
    stall.details = Some("manager (manager) stalled after 5m: inbox batching".to_string());
    sink.emit(stall).unwrap();

    let health = agent_health_by_member(tmp.path(), &[manager("manager"), engineer("eng-1")]);
    let manager_health = health.get("manager").unwrap();
    assert!(
        manager_health.has_supervisory_warning(),
        "manager should retain a stall warning from the current session",
    );
}

#[test]
fn build_team_status_health_counts_unhealthy_backend() {
    let rows = vec![TeamStatusRow {
        name: "eng-bad".to_string(),
        role: "engineer".to_string(),
        role_type: "Engineer".to_string(),
        agent: Some("claude".to_string()),
        reports_to: Some("manager".to_string()),
        state: "working".to_string(),
        pending_inbox: 0,
        triage_backlog: 0,
        active_owned_tasks: Vec::new(),
        review_owned_tasks: Vec::new(),
        signal: None,
        runtime_label: Some("working".to_string()),
        worktree_staleness: None,
        health: AgentHealthSummary {
            backend_health: crate::agent::BackendHealth::Unreachable,
            ..AgentHealthSummary::default()
        },
        health_summary: "B:unreachable".to_string(),
        eta: "-".to_string(),
    }];
    let health = build_team_status_health(&rows, true, false);
    assert_eq!(health.unhealthy_members, vec!["eng-bad".to_string()]);
}

#[test]
fn build_team_status_health_counts_supervisory_stall_warning() {
    let rows = vec![TeamStatusRow {
        name: "eng-stalled".to_string(),
        role: "engineer".to_string(),
        role_type: "Engineer".to_string(),
        agent: Some("codex".to_string()),
        reports_to: Some("manager".to_string()),
        state: "working".to_string(),
        pending_inbox: 0,
        triage_backlog: 0,
        active_owned_tasks: Vec::new(),
        review_owned_tasks: Vec::new(),
        signal: None,
        runtime_label: Some("working".to_string()),
        worktree_staleness: None,
        health: AgentHealthSummary {
            stall_reason: Some("supervisory_stalled_manager_no_actionable_progress".to_string()),
            stall_summary: Some(
                "eng-stalled (manager) stalled after 5m: no actionable progress".to_string(),
            ),
            ..AgentHealthSummary::default()
        },
        health_summary: "stall:manager:no-progress".to_string(),
        eta: "-".to_string(),
    }];

    let health = build_team_status_health(&rows, true, false);
    assert_eq!(health.unhealthy_members, vec!["eng-stalled".to_string()]);
}

// --- SQLite telemetry migration tests ---

#[test]
fn compute_metrics_with_telemetry_db_returns_review_metrics() {
    let tmp = tempfile::tempdir().unwrap();

    // Set up board with a task.
    write_board_task(
        tmp.path(),
        "001-task.md",
        "id: 1\ntitle: Test\nstatus: todo\npriority: high\n",
    );

    // Set up in-memory telemetry DB with events.
    let conn = crate::team::telemetry_db::open_in_memory().unwrap();
    let events = vec![
        crate::team::events::TeamEvent::task_completed("eng-1", Some("1")),
        crate::team::events::TeamEvent::task_auto_merged_with_mode(
            "eng-1",
            "1",
            0.9,
            3,
            50,
            Some(crate::team::merge::MergeMode::DirectRoot),
        ),
        crate::team::events::TeamEvent::task_completed("eng-1", Some("2")),
        crate::team::events::TeamEvent::task_manual_merged_with_mode(
            "2",
            Some(crate::team::merge::MergeMode::IsolatedIntegration),
        ),
        crate::team::events::TeamEvent::task_merge_failed(
            "eng-1",
            "3",
            Some(crate::team::merge::MergeMode::IsolatedIntegration),
            "isolated merge path failed: integration checkout broke",
        ),
        crate::team::events::TeamEvent::task_reworked("eng-1", "3"),
    ];
    for event in &events {
        crate::team::telemetry_db::insert_event(&conn, event).unwrap();
    }

    let metrics =
        compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], Some(&conn), None).unwrap();

    assert_eq!(metrics.auto_merge_count, 1);
    assert_eq!(metrics.manual_merge_count, 1);
    assert_eq!(metrics.direct_root_merge_count, 1);
    assert_eq!(metrics.isolated_integration_merge_count, 1);
    assert_eq!(metrics.direct_root_failure_count, 0);
    assert_eq!(metrics.isolated_integration_failure_count, 1);
    assert_eq!(metrics.rework_count, 1);
    assert!(metrics.auto_merge_rate.is_some());
    // Board metric still works.
    assert_eq!(metrics.runnable_count, 1);
}

#[test]
fn compute_metrics_without_db_falls_back_to_jsonl() {
    let tmp = tempfile::tempdir().unwrap();

    write_board_task(
        tmp.path(),
        "001-task.md",
        "id: 1\ntitle: Test\nstatus: todo\npriority: high\n",
    );

    // Write events to JSONL (no DB).
    let events_path = team_events_path(tmp.path());
    let mut sink = EventSink::new(&events_path).unwrap();
    sink.emit(TeamEvent::task_auto_merged_with_mode(
        "eng-1",
        "1",
        0.9,
        3,
        50,
        Some(crate::team::merge::MergeMode::DirectRoot),
    ))
    .unwrap();
    sink.emit(TeamEvent::task_merge_failed(
        "eng-1",
        "2",
        Some(crate::team::merge::MergeMode::IsolatedIntegration),
        "isolated merge path failed: integration checkout broke",
    ))
    .unwrap();

    let metrics =
        compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], None, Some(&events_path))
            .unwrap();

    assert_eq!(metrics.auto_merge_count, 1);
    assert_eq!(metrics.direct_root_merge_count, 1);
    assert_eq!(metrics.isolated_integration_failure_count, 1);
    assert_eq!(metrics.runnable_count, 1);
}

#[test]
fn compute_metrics_without_db_or_events_returns_zero_review_metrics() {
    let tmp = tempfile::tempdir().unwrap();

    write_board_task(
        tmp.path(),
        "001-task.md",
        "id: 1\ntitle: Test\nstatus: todo\npriority: high\n",
    );

    // No DB, no events path.
    let metrics = compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], None, None).unwrap();

    assert_eq!(metrics.auto_merge_count, 0);
    assert_eq!(metrics.manual_merge_count, 0);
    assert_eq!(metrics.direct_root_merge_count, 0);
    assert_eq!(metrics.isolated_integration_merge_count, 0);
    assert_eq!(metrics.direct_root_failure_count, 0);
    assert_eq!(metrics.isolated_integration_failure_count, 0);
    assert_eq!(metrics.rework_count, 0);
    assert_eq!(metrics.auto_merge_rate, None);
    // Board metric still works.
    assert_eq!(metrics.runnable_count, 1);
}

#[test]
fn compute_metrics_excludes_manual_blocked_todo_from_runnable_count() {
    let tmp = tempfile::tempdir().unwrap();

    write_board_task(
        tmp.path(),
        "001-dispatchable-a.md",
        "id: 1\ntitle: Dispatchable A\nstatus: todo\npriority: high\n",
    );
    write_board_task(
        tmp.path(),
        "002-dispatchable-b.md",
        "id: 2\ntitle: Dispatchable B\nstatus: todo\npriority: high\n",
    );
    write_board_task(
        tmp.path(),
        "003-manual-blocked.md",
        "id: 3\ntitle: Manual Blocked\nstatus: todo\npriority: high\nblocked: manual provider-console token rotation\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();
    assert_eq!(metrics.runnable_count, 2);
    assert_eq!(metrics.blocked_count, 1);
}

#[test]
fn compute_metrics_excludes_done_verification_retry_from_runnable_count() {
    let tmp = tempfile::tempdir().unwrap();

    write_board_task(
        tmp.path(),
        "695-done-retry-a.md",
        "id: 695\ntitle: Done retry A\nstatus: done\npriority: high\ntests_passed: false\noutcome: verification_retry_required\nartifacts:\n  - artifacts/retry-695.json\n",
    );
    write_board_task(
        tmp.path(),
        "701-done-retry-b.md",
        "id: 701\ntitle: Done retry B\nstatus: done\npriority: high\ntests_passed: false\noutcome: verification_retry_required\nartifacts:\n  - artifacts/retry-701.json\n",
    );
    write_board_task(
        tmp.path(),
        "742-done-retry-c.md",
        "id: 742\ntitle: Done retry C\nstatus: done\npriority: high\ntests_passed: false\noutcome: verification_retry_required\nartifacts:\n  - artifacts/retry-742.json\n",
    );
    write_board_task(
        tmp.path(),
        "900-open-retry.md",
        "id: 900\ntitle: Open retry\nstatus: todo\npriority: high\ntests_passed: false\noutcome: verification_retry_required\nartifacts:\n  - artifacts/retry-900.json\n",
    );

    let metrics = compute_metrics(&board_dir(tmp.path()), &[engineer("eng-1")]).unwrap();

    assert_eq!(metrics.runnable_count, 1);
    assert_eq!(metrics.top_runnable_tasks, vec!["#900 (high) Open retry"]);
}

/// Regression for #618: when a supervisor has actionable backlog
/// (`needs review` / `needs triage`), generic stall text must be
/// suppressed so operators see the actionable reason first.
#[test]
fn merge_status_signal_suppresses_generic_stall_when_actionable_backlog_present() {
    let mut review_pressure = SupervisoryPressureSnapshot::default();
    review_pressure.add_pressure(
        crate::team::supervisory_notice::SupervisoryPressure::ReviewBacklog,
        2,
    );
    let result = merge_status_signal(
        None,
        None,
        None,
        Some("manager stall after 32m".to_string()),
        &review_pressure,
        0,
    );
    assert_eq!(
        result,
        Some("pressure 1: review backlog (2)".to_string()),
        "generic stall text should be suppressed when review backlog is non-zero"
    );

    let mut triage_pressure = SupervisoryPressureSnapshot::default();
    triage_pressure.add_pressure(
        crate::team::supervisory_notice::SupervisoryPressure::TriageBacklog,
        3,
    );
    let result = merge_status_signal(
        None,
        None,
        None,
        Some("architect stall after 15m".to_string()),
        &triage_pressure,
        0,
    );
    assert_eq!(
        result,
        Some("pressure 1: direct-report packets (3)".to_string()),
        "generic stall text should be suppressed when triage backlog is non-zero"
    );

    let result = merge_status_signal(
        None,
        None,
        None,
        Some("manager stall after 42m".to_string()),
        &SupervisoryPressureSnapshot::default(),
        0,
    );
    assert_eq!(
        result,
        Some("manager stall after 42m".to_string()),
        "generic stall text should remain when no actionable backlog is present"
    );
}

#[test]
fn format_metrics_unchanged_with_db_source() {
    let conn = crate::team::telemetry_db::open_in_memory().unwrap();
    let events = vec![
        crate::team::events::TeamEvent::task_completed("eng-1", Some("1")),
        crate::team::events::TeamEvent::task_auto_merged("eng-1", "1", 0.9, 3, 50),
    ];
    for event in &events {
        crate::team::telemetry_db::insert_event(&conn, event).unwrap();
    }

    let tmp = tempfile::tempdir().unwrap();
    write_board_task(
        tmp.path(),
        "001-task.md",
        "id: 1\ntitle: Test\nstatus: review\npriority: high\nclaimed_by: eng-1\n",
    );

    let metrics =
        compute_metrics_with_telemetry(&board_dir(tmp.path()), &[], Some(&conn), None).unwrap();

    let formatted = format_metrics(&metrics);
    assert!(formatted.contains("Workflow Metrics"));
    assert!(formatted.contains("Auto-merge Rate: 100%"));
    assert!(formatted.contains("In Review: 1"));
}
