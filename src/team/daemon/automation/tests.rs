use super::super::*;
use super::{
    ClaimedTaskBranchMismatch, active_stale_review_entries, authoritative_task_branch,
    select_authoritative_multi_claim_task, task_claim_progress_type,
};
use crate::team::config::RoleType;
use crate::team::config::{BoardConfig, WorkflowMode, WorkflowPolicy};
use crate::team::events::TeamEvent;
use crate::team::hierarchy::MemberInstance;
use crate::team::task_cmd::{
    set_optional_string, set_optional_u64, update_task_frontmatter, yaml_key,
};
use crate::team::task_loop::{
    current_worktree_branch, engineer_base_branch_name, setup_engineer_worktree,
};
use crate::team::test_helpers::{make_test_daemon, write_event_log};
use crate::team::test_support::{
    EnvVarGuard, TestDaemonBuilder, architect_member, engineer_member, env_lock, git_ok,
    git_stdout, init_git_repo, manager_member, write_board_task_file, write_open_task_file,
    write_owned_task_file, write_owned_task_file_with_context,
};
use std::collections::HashMap;
use std::path::Path;

fn test_task(
    id: u32,
    status: &str,
    claimed_by: Option<&str>,
    review_owner: Option<&str>,
) -> crate::task::Task {
    crate::task::Task {
        id,
        title: format!("task-{id}"),
        status: status.to_string(),
        priority: "high".to_string(),
        assignee: None,
        claimed_by: claimed_by.map(str::to_string),
        claimed_at: None,
        claim_ttl_secs: None,
        claim_expires_at: None,
        last_progress_at: None,
        claim_warning_sent_at: None,
        claim_extensions: None,
        last_output_bytes: None,
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
        review_owner: review_owner.map(str::to_string),
        blocked_on: None,
        worktree_path: None,
        branch: None,
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: std::path::PathBuf::new(),
    }
}

fn setup_fake_kanban_for_planning(
    tmp: &tempfile::TempDir,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let fake_bin = tmp.path().join("fake-bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let log_path = tmp.path().join("kanban.log");
    let script = fake_bin.join("kanban-md");
    std::fs::write(
        &script,
        format!(
            "#!/bin/bash
set -euo pipefail
printf '%s\\n' \"$*\" >> '{}'
if [ \"$1\" != \"create\" ]; then
  exit 1
fi
title=\"$2\"
body=\"\"
priority=\"high\"
tags=\"\"
depends_on=\"\"
shift 2
while [ $# -gt 0 ]; do
  case \"$1\" in
--body) body=\"$2\"; shift 2 ;;
--priority) priority=\"$2\"; shift 2 ;;
--tags) tags=\"$2\"; shift 2 ;;
--depends-on) depends_on=\"$2\"; shift 2 ;;
--dir) board_dir=\"$2\"; shift 2 ;;
*) shift ;;
  esac
done
mkdir -p \"$board_dir/tasks\"
count=$(find \"$board_dir/tasks\" -maxdepth 1 -name '*.md' | wc -l | tr -d ' ')
id=$((count + 1))
printf -- '---\nid: %s\ntitle: %s\nstatus: todo\npriority: %s\n' \"$id\" \"$title\" \"$priority\" > \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
if [ -n \"$tags\" ]; then
  printf 'tags: [%s]\n' \"$tags\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
fi
if [ -n \"$depends_on\" ]; then
  printf 'depends_on: [%s]\n' \"$depends_on\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
fi
printf -- '---\n\n%s\n' \"$body\" >> \"$board_dir/tasks/$(printf '%03d' \"$id\")-task.md\"
printf 'Created task #%s\n' \"$id\"
",
            log_path.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (fake_bin, log_path)
}

fn planning_test_daemon(tmp: &tempfile::TempDir, cooldown_secs: u64) -> TeamDaemon {
    let board_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&board_dir).unwrap();

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![
            architect_member("architect"),
            engineer_member("eng-1", Some("architect"), false),
            engineer_member("eng-2", Some("architect"), false),
            engineer_member("eng-3", Some("architect"), false),
        ])
        .workflow_policy(WorkflowPolicy {
            planning_cycle_cooldown_secs: cooldown_secs,
            ..WorkflowPolicy::default()
        })
        .states(HashMap::from([
            ("architect".to_string(), MemberState::Idle),
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
            ("eng-3".to_string(), MemberState::Idle),
        ]))
        .build();
    daemon.config.team_config.workflow_mode = WorkflowMode::Hybrid;
    daemon
}

fn planning_inbox_messages(tmp: &tempfile::TempDir) -> Vec<inbox::InboxMessage> {
    inbox::pending_messages(&inbox::inboxes_root(tmp.path()), "architect").unwrap()
}

fn write_review_backlog_task(project_root: &Path) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("736-review-backlog.md"),
        "---\n\
id: 736\n\
title: Review retry branch completion\n\
status: review\n\
priority: high\n\
claimed_by: eng-1-2\n\
review_owner: manager\n\
branch: eng-1-2/736\n\
commit: abc1234\n\
artifacts:\n\
  - .batty/reports/verification/completion/task-736-eng-1-2-attempt-1.json\n\
next_action: review branch and disposition retry\n\
class: standard\n\
---\n\nTask description.\n",
    )
    .unwrap();
}

fn write_planning_docs(tmp: &tempfile::TempDir) {
    let planning_dir = tmp.path().join("planning");
    std::fs::create_dir_all(&planning_dir).unwrap();
    std::fs::write(
        planning_dir.join("roadmap.md"),
        "# Roadmap\n\nPhase 2: Productionize tact planning.\n- Auto-dispatch new tact tasks.\n",
    )
    .unwrap();
    std::fs::write(
        planning_dir.join("architecture.md"),
        "# Architecture\n\nGoal: Keep idle engineers fed with executable work.\n- Prefer small end-to-end automation loops.\n",
    )
    .unwrap();
}

fn board_task_path(project_root: &Path, task_id: u32) -> std::path::PathBuf {
    crate::task::find_task_path_by_id(
        &project_root
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
        task_id,
    )
    .unwrap()
}

fn mark_verification_retry_required(project_root: &Path, task_id: u32, artifact: &str) {
    let task_path = board_task_path(project_root, task_id);
    update_task_frontmatter(&task_path, |mapping| {
        mapping.insert(yaml_key("tests_run"), serde_yaml::Value::Bool(true));
        mapping.insert(yaml_key("tests_passed"), serde_yaml::Value::Bool(false));
        set_optional_string(mapping, "outcome", Some("verification_retry_required"));
        mapping.insert(
            yaml_key("artifacts"),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(artifact.to_string())]),
        );
    })
    .unwrap();
}

const SINGLE_TASK_RESPONSE: &str = r#"---
title: "Add planning telemetry"
priority: high
tags: [tact, telemetry]
---
Record planning cycle events in the orchestrator log and add assertions.
"#;

const THREE_TASK_RESPONSE: &str = r#"---
title: "Add planning telemetry"
priority: high
tags: [tact, telemetry]
---
Record planning cycle events in the orchestrator log.
---
title: "Persist planning outputs"
priority: medium
depends_on: [1]
tags: [tact, board]
---
Create board tasks from planning responses.
---
title: "Backfill planning tests"
priority: medium
depends_on: [1, 2]
tags: [tact, tests]
---
Add end-to-end planning cycle coverage.
"#;

const MIXED_RESPONSE: &str = r#"---
title: "Good task"
priority: high
tags: [tact]
---
Good body.
---
title: [broken
---
Bad body.
---
title: "Second good task"
priority: medium
depends_on: [1]
---
Second body.
"#;

const RAW_LOG_RESPONSE: &str = r#"---
title: "Valid planning task"
priority: high
tags: [tact]
---
Write a real planning task body.
---
title: "Rejected raw log task"
priority: high
tags: [tact]
---
running 3144 tests
test tmux::tests::split_window_horizontal_creates_new_pane ... FAILED
thread 'tmux::tests::split_window_horizontal_creates_new_pane' panicked at src/tmux.rs:42:9
"#;

#[test]
fn planning_cycle_trigger_generates_prompt_in_architect_inbox() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);

    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]
            .body
            .contains("HIGH PRIORITY: planning cycle triggered")
    );
    assert!(messages[0].body.contains("Idle engineers available: 3"));
    assert!(messages[0].body.contains("Expected response format:"));
    assert!(daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_cycle_prompt_includes_recent_completion_context() {
    let tmp = tempfile::tempdir().unwrap();
    write_event_log(
        tmp.path(),
        &[TeamEvent::task_completed("eng-2", Some("44"))],
    );

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].body.contains("eng-2 completed task #44"));
}

#[test]
fn planning_cycle_prompt_uses_truly_idle_engineer_count() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);

    write_board_task_file(
        tmp.path(),
        201,
        "eng-1-active-review",
        "review",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].body.contains("Idle engineers available: 2"));
    assert!(
        messages[0]
            .body
            .contains("Propose exactly 1 review-drain unblock task.")
    );
}

#[test]
fn planning_cycle_does_not_trigger_when_dispatchable_work_is_sufficient() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    write_board_task_file(tmp.path(), 301, "ready-a", "todo", None, &[], None);
    write_board_task_file(tmp.path(), 302, "ready-b", "todo", None, &[], None);
    write_board_task_file(tmp.path(), 303, "ready-c", "todo", None, &[], None);

    daemon.maybe_trigger_planning_cycle().unwrap();

    assert!(planning_inbox_messages(&tmp).is_empty());
    assert!(!daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_cycle_triggers_when_todo_exists_but_no_work_is_dispatchable() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    write_board_task_file(
        tmp.path(),
        401,
        "blocked-by-dependency",
        "todo",
        None,
        &[999],
        None,
    );

    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 1);
    assert!(messages[0].body.contains("dispatchable_tasks=0"));
    assert!(messages[0].body.contains("Propose exactly 3 task(s)."));
    assert!(daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_cycle_blocked_only_prompt_requests_executable_unblock_work() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    write_board_task_file(
        tmp.path(),
        737,
        "blocked-only-cycle",
        "blocked",
        None,
        &[],
        Some("waiting on concrete unblock plan"),
    );

    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 1);
    assert!(
        messages[0]
            .body
            .contains("planning_state=blocked-only-board")
    );
    assert!(
        messages[0]
            .body
            .contains("#737 blocked-only-cycle: waiting on concrete unblock plan")
    );
    assert!(
        messages[0]
            .body
            .contains("Propose exactly 3 executable unblock task(s).")
    );
    assert!(
        messages[0]
            .body
            .contains("Each proposed task must name the blocked task id")
    );
}

#[test]
fn planning_cycle_review_backlog_prompt_requests_review_drain_unblock_work() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    write_review_backlog_task(tmp.path());
    write_board_task_file(
        tmp.path(),
        738,
        "blocked-release",
        "blocked",
        None,
        &[],
        Some("waiting on #736 review disposition"),
    );
    write_board_task_file(
        tmp.path(),
        739,
        "blocked-dispatch",
        "blocked",
        None,
        &[],
        Some("waiting on #736 review disposition"),
    );

    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 1);
    let body = &messages[0].body;
    assert!(body.contains("planning_state=review-backlog-gated"));
    assert!(body.contains("Review backlog requiring drain/unblock tasks:"));
    assert!(body.contains("#736 Review retry branch completion"));
    assert!(body.contains("review_owner=manager"));
    assert!(body.contains("branch=eng-1-2/736"));
    assert!(body.contains("commit=abc1234"));
    assert!(body.contains("task-736-eng-1-2-attempt-1.json"));
    assert!(body.contains("next_action=review branch and disposition retry"));
    assert!(body.contains("#738 blocked-release"));
    assert!(body.contains("#739 blocked-dispatch"));
    assert!(body.contains("Propose exactly 1 review-drain unblock task."));
    assert!(body.contains("depends_on set to the review task id"));
}

#[test]
fn planning_cycle_mixed_blocked_and_runnable_board_does_not_trigger() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    write_board_task_file(
        tmp.path(),
        738,
        "blocked",
        "blocked",
        None,
        &[],
        Some("waiting"),
    );
    write_board_task_file(tmp.path(), 739, "ready-a", "todo", None, &[], None);
    write_board_task_file(tmp.path(), 740, "ready-b", "todo", None, &[], None);
    write_board_task_file(tmp.path(), 741, "ready-c", "todo", None, &[], None);

    daemon.maybe_trigger_planning_cycle().unwrap();

    assert!(planning_inbox_messages(&tmp).is_empty());
    assert!(!daemon.scheduler.planning_cycle_active);
}

#[test]
fn claim_progress_commit_detection_uses_configured_trunk_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-progress-mainline");
    git_ok(&repo, &["checkout", "-b", "mainline"]);
    git_ok(&repo, &["checkout", "-b", "eng-1/task-42"]);
    std::fs::write(repo.join("progress.txt"), "task progress\n").unwrap();
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "task progress"]);
    git_ok(&repo, &["checkout", "main"]);
    git_ok(
        &repo,
        &[
            "merge",
            "--no-ff",
            "eng-1/task-42",
            "-m",
            "merge task progress to main",
        ],
    );
    git_ok(&repo, &["checkout", "eng-1/task-42"]);

    let mut task = test_task(42, "in-progress", Some("eng-1"), None);
    task.last_progress_at = Some("2000-01-01T00:00:00Z".to_string());

    assert_eq!(
        task_claim_progress_type(&task, &repo, 0, "mainline"),
        Some("commit")
    );
    assert_eq!(task_claim_progress_type(&task, &repo, 0, "main"), None);
}

#[test]
fn planning_cycle_round_trip_creates_board_tasks_and_resets_active_flag() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _path_guard = EnvVarGuard::set("PATH", &path);

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.maybe_trigger_planning_cycle().unwrap();
    let created = daemon
        .handle_planning_response(THREE_TASK_RESPONSE)
        .unwrap();

    assert_eq!(created, 3);
    assert!(!daemon.scheduler.planning_cycle_active);

    let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(tasks[0].title, "Add planning telemetry");
    assert_eq!(tasks[1].title, "Persist planning outputs");
    assert_eq!(tasks[2].title, "Backfill planning tests");
    assert_eq!(tasks[0].tags, vec!["tact", "telemetry"]);
    assert_eq!(tasks[1].depends_on, vec![1]);
    assert_eq!(tasks[1].tags, vec!["tact", "board"]);
    assert_eq!(tasks[2].depends_on, vec![1, 2]);
    assert_eq!(tasks[2].tags, vec!["tact", "tests"]);

    let orchestrator_log =
        std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap_or_default();
    assert!(orchestrator_log.contains("planning: triggered planning cycle"));
    assert!(orchestrator_log.contains("planning: applied planning response and created 3 tasks"));

    let events =
        crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "tact_cycle_triggered"
            && event.role.as_deref() == Some("architect")
            && event.working_members == Some(3)
    }));
    assert!(events.iter().any(|event| {
        event.event == "tact_tasks_created"
            && event.role.as_deref() == Some("architect")
            && event.restart_count == Some(3)
            && event.reason.as_deref() == Some("success")
    }));
}

#[test]
fn planning_round_trip_with_malformed_blocks_keeps_good_tasks() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _path_guard = EnvVarGuard::set("PATH", &path);

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.scheduler.planning_cycle_active = true;

    let created = daemon.handle_planning_response(MIXED_RESPONSE).unwrap();
    assert_eq!(created, 2);
    assert!(!daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_round_trip_rejects_raw_log_generated_tasks() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _path_guard = EnvVarGuard::set("PATH", &path);

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.scheduler.planning_cycle_active = true;

    let created = daemon.handle_planning_response(RAW_LOG_RESPONSE).unwrap();
    assert_eq!(created, 1);
    assert!(!daemon.scheduler.planning_cycle_active);

    let tasks = crate::task::load_tasks_from_dir(
        &tmp.path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )
    .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "Valid planning task");
}

#[test]
fn planning_response_empty_creates_zero_tasks_and_resets_cycle() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.scheduler.planning_cycle_active = true;

    let created = daemon.handle_planning_response("").unwrap();
    assert_eq!(created, 0);
    assert!(!daemon.scheduler.planning_cycle_active);

    let orchestrator_log =
        std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap_or_default();
    assert!(orchestrator_log.contains("planning: applied planning response and created 0 tasks"));
}

#[test]
fn empty_planning_response_persists_consecutive_empty_increment() {
    // #710: `handle_planning_response` used to update
    // `planning_cycle_consecutive_empty` and `planning_cycle_last_fired`
    // in memory only. A non-clean daemon termination between the
    // response-applied heartbeat and the next 5-min heartbeat would
    // revert those values to the last persisted snapshot — shrinking
    // the backoff cooldown so a spurious planning cycle could fire
    // within minutes of the restart. Observed in batty-marketing at
    // 12:42:49 UTC; this test locks in the persist-after-apply.
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.scheduler.planning_cycle_active = true;
    daemon.scheduler.planning_cycle_consecutive_empty = 1;

    daemon.handle_planning_response("").unwrap();
    assert_eq!(daemon.scheduler.planning_cycle_consecutive_empty, 2);

    let persisted = super::super::state::load_daemon_state(tmp.path())
        .expect("daemon state should be persisted after empty planning response");
    assert_eq!(persisted.planning_cycle_consecutive_empty, 2);
    assert!(
        persisted.planning_cycle_last_fired_elapsed_secs.is_some(),
        "last_fired should be recorded as the cooldown anchor"
    );
}

#[test]
fn planning_response_missing_board_dir_returns_graceful_error_and_resets_cycle() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _path_guard = EnvVarGuard::set("PATH", &path);

    let mut daemon = planning_test_daemon(&tmp, 300);
    std::fs::remove_dir_all(daemon.board_dir()).unwrap();
    daemon.scheduler.planning_cycle_active = true;

    let error = daemon
        .handle_planning_response(SINGLE_TASK_RESPONSE)
        .unwrap_err();
    assert!(error.to_string().contains("board directory does not exist"));
    assert!(!daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_response_missing_kanban_binary_returns_clear_error() {
    if !crate::team::test_support::run_in_isolated_process(
        concat!(
            module_path!(),
            "::planning_response_missing_kanban_binary_returns_clear_error"
        ),
        &[("PATH", "/definitely/missing")],
    ) {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.scheduler.planning_cycle_active = true;

    let error = daemon
        .handle_planning_response(SINGLE_TASK_RESPONSE)
        .unwrap_err();
    let detail = error.to_string();
    assert!(detail.contains("failed to create board task") || detail.contains("failed to execute"));
    assert!(!daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_response_double_apply_is_graceful() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _path_guard = EnvVarGuard::set("PATH", &path);

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon.scheduler.planning_cycle_active = true;

    assert_eq!(
        daemon
            .handle_planning_response(SINGLE_TASK_RESPONSE)
            .unwrap(),
        1
    );
    assert_eq!(
        daemon
            .handle_planning_response(SINGLE_TASK_RESPONSE)
            .unwrap(),
        0
    );

    let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(!daemon.scheduler.planning_cycle_active);
}

#[test]
fn planning_cycle_cooldown_blocks_immediate_retrigger() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);

    daemon.maybe_trigger_planning_cycle().unwrap();
    daemon.scheduler.planning_cycle_active = false;
    daemon.maybe_trigger_planning_cycle().unwrap();

    assert_eq!(planning_inbox_messages(&tmp).len(), 1);
}

#[test]
fn planning_cycle_retriggers_after_cooldown_expires() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = planning_test_daemon(&tmp, 300);

    daemon.maybe_trigger_planning_cycle().unwrap();
    daemon.scheduler.planning_cycle_active = false;
    daemon.scheduler.planning_cycle_last_fired = Some(Instant::now() - Duration::from_secs(301));
    daemon.maybe_trigger_planning_cycle().unwrap();

    assert_eq!(planning_inbox_messages(&tmp).len(), 2);
}

#[test]
fn planning_prompt_and_parser_round_trip_sample_response() {
    let prompt = crate::team::tact::compose_planning_prompt(
        3,
        "todo=0 backlog=2 in-progress=1 review=0 done=3 idle_engineers=3",
        &["eng-1 completed task #44".to_string()],
        &["Improve planning automation".to_string()],
        &["Improve planning automation".to_string()],
        "Batty",
    );

    let specs = crate::team::tact::parse_planning_response(THREE_TASK_RESPONSE);
    assert!(prompt.contains("Improve planning automation"));
    assert_eq!(specs.len(), 3);
    assert_eq!(specs[2].depends_on, vec![1, 2]);
}

#[test]
#[cfg_attr(not(feature = "integration"), ignore)]
fn planning_cycle_end_to_end_includes_context_and_dispatches_created_work() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    write_planning_docs(&tmp);
    let (fake_bin, _log_path) = setup_fake_kanban_for_planning(&tmp);
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let _path_guard = EnvVarGuard::set("PATH", &path);

    let mut daemon = planning_test_daemon(&tmp, 300);
    daemon
        .config
        .team_config
        .workflow_policy
        .pipeline_starvation_threshold = Some(1);
    daemon
        .config
        .team_config
        .board
        .dispatch_stabilization_delay_secs = 0;
    daemon.scheduler.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    for engineer in ["eng-1", "eng-2", "eng-3"] {
        daemon.idle_started_at.insert(
            engineer.to_string(),
            Instant::now() - Duration::from_secs(60),
        );
    }

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(daemon.scheduler.pipeline_starvation_fired);

    daemon.maybe_trigger_planning_cycle().unwrap();

    let messages = planning_inbox_messages(&tmp);
    assert_eq!(messages.len(), 2);
    let planning_prompt = messages
        .iter()
        .find(|message| message.body.contains("Expected response format:"))
        .map(|message| message.body.as_str())
        .expect("expected planning prompt in architect inbox");
    assert!(planning_prompt.contains("Phase 2: Productionize tact planning."));
    assert!(planning_prompt.contains("Auto-dispatch new tact tasks."));
    assert!(planning_prompt.contains("Goal: Keep idle engineers fed with executable work."));

    let created = daemon
        .handle_planning_response(THREE_TASK_RESPONSE)
        .unwrap();
    assert_eq!(created, 3);

    daemon.maybe_auto_dispatch().unwrap();

    let tasks = crate::task::load_tasks_from_dir(&daemon.board_dir().join("tasks")).unwrap();
    assert!(
        tasks
            .iter()
            .any(|task| task.status == "in-progress" && task.claimed_by.is_some()),
        "expected at least one created tact task to be dispatched"
    );
}

#[test]
fn maybe_auto_unblock_moves_blocked_task_to_todo_and_notifies_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
    let board_tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "eng-1").unwrap();

    write_board_task_file(tmp.path(), 11, "dep-a", "done", None, &[], None);
    write_board_task_file(tmp.path(), 12, "dep-b", "done", None, &[], None);
    write_board_task_file(
        tmp.path(),
        13,
        "blocked-task",
        "blocked",
        Some("eng-1"),
        &[11, 12],
        Some("waiting on dependencies"),
    );

    daemon.maybe_auto_unblock_blocked_tasks().unwrap();

    let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
    let task = tasks.iter().find(|task| task.id == 13).unwrap();
    assert_eq!(task.status, "todo");
    assert!(task.claimed_by.is_none());
    assert!(task.blocked_on.is_none());
    assert!(task.blocked.is_none());

    let pending = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("Task #13 (blocked-task)"));
    assert!(
        pending[0]
            .body
            .contains("automatically moved from `blocked` to `todo`")
    );
    assert!(pending[0].body.contains("[11, 12]"));

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_unblocked"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("13")
    }));
}

#[test]
fn maybe_auto_unblock_clears_stale_claim_from_todo_dependency_task() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    let board_tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");

    write_board_task_file(tmp.path(), 41, "dep-a", "done", None, &[], None);
    write_board_task_file(
        tmp.path(),
        42,
        "todo-child",
        "todo",
        Some("eng-1"),
        &[41],
        Some("waiting on dependencies"),
    );

    daemon.maybe_auto_unblock_blocked_tasks().unwrap();

    let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
    let task = tasks.iter().find(|task| task.id == 42).unwrap();
    assert_eq!(task.status, "todo");
    assert!(task.claimed_by.is_none());
    assert!(task.blocked_on.is_none());
    assert!(task.blocked.is_none());
}

#[test]
fn maybe_auto_unblock_notifies_manager_when_task_is_unowned() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
    let inbox_root = inbox::inboxes_root(tmp.path());
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    inbox::init_inbox(&inbox_root, "manager").unwrap();

    write_board_task_file(tmp.path(), 21, "dep-a", "done", None, &[], None);
    write_board_task_file(
        tmp.path(),
        22,
        "blocked-task",
        "blocked",
        None,
        &[21],
        Some("waiting on dependencies"),
    );

    daemon.maybe_auto_unblock_blocked_tasks().unwrap();

    let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].body.contains("Task #22 (blocked-task)"));

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_unblocked"
            && event.role.as_deref() == Some("manager")
            && event.task.as_deref() == Some("22")
    }));
}

#[test]
fn maybe_auto_unblock_leaves_unresolved_or_dependency_free_tasks_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![manager]);
    let board_tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "manager").unwrap();

    write_board_task_file(tmp.path(), 31, "dep-a", "done", None, &[], None);
    write_board_task_file(tmp.path(), 32, "dep-b", "review", None, &[], None);
    write_board_task_file(
        tmp.path(),
        33,
        "blocked-partial",
        "blocked",
        None,
        &[31, 32],
        Some("waiting on dependencies"),
    );
    write_board_task_file(
        tmp.path(),
        34,
        "blocked-no-deps",
        "blocked",
        None,
        &[],
        Some("manual hold"),
    );

    daemon.maybe_auto_unblock_blocked_tasks().unwrap();

    let tasks = crate::task::load_tasks_from_dir(&board_tasks_dir).unwrap();
    let partial = tasks.iter().find(|task| task.id == 33).unwrap();
    assert_eq!(partial.status, "blocked");
    assert_eq!(
        partial.blocked_on.as_deref(),
        Some("waiting on dependencies")
    );

    let no_deps = tasks.iter().find(|task| task.id == 34).unwrap();
    assert_eq!(no_deps.status, "blocked");
    assert_eq!(no_deps.blocked_on.as_deref(), Some("manual hold"));

    let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(pending.is_empty());

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.task.as_deref(), Some("33" | "34")))
    );
}

#[test]
fn auto_retro_fires_when_all_done() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
    write_event_log(
        tmp.path(),
        &[
            TeamEvent::daemon_started(),
            TeamEvent::task_assigned("eng-1", "45"),
            TeamEvent::task_completed("eng-1", Some("45")),
            TeamEvent::daemon_stopped(),
        ],
    );

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .orchestrator_pane(false)
        .build();
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    daemon.maybe_generate_retrospective().unwrap();

    assert!(daemon.scheduler.retro_generated);
    let retro_dir = tmp.path().join(".batty").join("retrospectives");
    let reports = std::fs::read_dir(&retro_dir).unwrap().count();
    assert_eq!(reports, 1);

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == "retro_generated")
            .count(),
        1
    );
}

// ── reconcile_active_tasks ──────────────────────────────────────

/// Regression for #630: when a task is already done/approved on
/// the board but the engineer worktree is still tracked in the
/// daemon's active_tasks map (the "post-approval dirty lane"
/// case), reconcile must clear the engineer so a fresh dispatch
/// can run. Without this, completed lanes stay parked forever
/// waiting on a human intervention.
#[test]
fn reconcile_active_tasks_recovers_post_approval_dirty_lane() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 628);

    // Task #628 is done on the board — the engineer's active_task
    // is pointing at completed work.
    write_board_task_file(
        tmp.path(),
        628,
        "completed-task",
        "done",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();

    assert!(
        !daemon.active_tasks.contains_key("eng-1"),
        "engineer must be freed from the post-approval dirty lane"
    );
}

#[test]
fn reconcile_active_tasks_clears_done_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "done-task",
        "done",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();
    daemon.reconcile_active_tasks().unwrap();
    assert!(!daemon.active_tasks.contains_key("eng-1"));

    let events =
        crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
    let clear_done_events = events
        .iter()
        .filter(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("10")
                && event.reason.as_deref() == Some("clear_done")
        })
        .count();
    assert_eq!(
        clear_done_events, 1,
        "done-task cleanup should emit one stale active audit event"
    );
}

#[test]
fn reconcile_active_tasks_clears_archived_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "archived-task",
        "archived",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();
    assert!(!daemon.active_tasks.contains_key("eng-1"));

    let events =
        crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("10")
            && event.reason.as_deref() == Some("clear_archived")
    }));
}

#[test]
fn reconcile_active_tasks_preserves_dirty_archived_lane_before_clear() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-archived-dirty-lane");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    std::fs::write(
        worktree_dir.join("archived-dirty.txt"),
        "archived dirty work\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "archived-dirty.txt"]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "archived-task",
        "archived",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.reconcile_active_tasks().unwrap();

    assert!(!daemon.active_tasks.contains_key("eng-1"));
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert!(
        !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
        "dirty archived-lane work should be preserved before the lane is reset"
    );
    let preserved_record =
        crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
    assert_eq!(preserved_record.task_id, 42);
    assert!(
        preserved_record.doctor_check_line().contains("eng-1/42"),
        "preservation record should name the archived task branch"
    );
}

#[test]
fn reconcile_active_tasks_clears_missing_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 999);

    // No task files exist at all — task 999 is missing from board
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    daemon.reconcile_active_tasks().unwrap();
    assert!(!daemon.active_tasks.contains_key("eng-1"));
}

#[test]
fn reconcile_active_tasks_keeps_in_progress_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "active-task",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();
    assert_eq!(daemon.active_tasks.get("eng-1"), Some(&10));
}

/// #698: regression — when the board records a role-name claim
/// (e.g. `eng`) but active_tasks is keyed on the instance name
/// (`eng-1-1`), reconciliation must resolve the role claim to its
/// sole engineer instance before flagging the entry as stale.
/// Pre-fix this loop fired every poll and cycled against the
/// auto-doctor post-hot-reload re-attachment at auto_doctor.rs:79.
#[test]
fn reconcile_active_tasks_resolves_role_name_claim_to_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "role-name-claim",
        "in-progress",
        Some("eng"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();
    assert_eq!(
        daemon.active_tasks.get("eng-1-1"),
        Some(&10),
        "role-name claim 'eng' must resolve to instance 'eng-1-1' and not flag stale"
    );
}

/// #698: guard — resolution must not be used when the role has
/// multiple engineer instances, because it becomes ambiguous.
/// In that case the comparison correctly flags stale since we
/// cannot prove the claim belongs to this specific instance.
#[test]
fn reconcile_active_tasks_flags_stale_when_role_is_ambiguous() {
    let tmp = tempfile::tempdir().unwrap();
    let eng_a = MemberInstance {
        name: "eng-1-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let eng_b = MemberInstance {
        name: "eng-1-2".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![eng_a, eng_b]);
    daemon.active_tasks.insert("eng-1-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "ambiguous-role-claim",
        "in-progress",
        Some("eng"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();
    assert_eq!(
        daemon.active_tasks.get("eng-1-1"),
        None,
        "ambiguous role claim must not silently match a specific instance"
    );
}

#[test]
fn reconcile_active_tasks_releases_review_tasks_without_losing_review_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "review-task",
        "review",
        Some("eng-1"),
        &[],
        None,
    );
    let task_path = board_task_path(tmp.path(), 10);
    crate::team::task_cmd::assign_task_owners(
        &daemon.board_dir(),
        10,
        Some("eng-1"),
        Some("manager"),
    )
    .unwrap();

    daemon.reconcile_active_tasks().unwrap();

    assert!(!daemon.active_tasks.contains_key("eng-1"));
    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "review");
    assert!(task.claimed_by.is_none());
    assert_eq!(task.review_owner.as_deref(), Some("manager"));

    let events = crate::team::events::read_events(
        &tmp.path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("10")
            && event.reason.as_deref() == Some("release_review")
    }));
}

#[test]
fn reconcile_active_tasks_releases_blocked_tasks_without_losing_block_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        10,
        "blocked-task",
        "blocked",
        Some("eng-1"),
        &[],
        Some("manual provider-console token rotation"),
    );
    let task_path = board_task_path(tmp.path(), 10);

    daemon.reconcile_active_tasks().unwrap();

    assert!(!daemon.active_tasks.contains_key("eng-1"));
    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "blocked");
    assert!(task.claimed_by.is_none());
    assert_eq!(
        task.blocked.as_deref(),
        Some("manual provider-console token rotation")
    );

    let events = crate::team::events::read_events(
        &tmp.path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("10")
            && event.reason.as_deref() == Some("release_blocked")
    }));
}

#[test]
fn reconcile_active_tasks_adopts_single_board_owned_task() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![engineer]);

    write_board_task_file(
        tmp.path(),
        20,
        "owned-task",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(20));

    let events = crate::team::events::read_events(
        &tmp.path()
            .join(".batty")
            .join("team_config")
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("20")
            && event.reason.as_deref() == Some("adopt")
    }));
}

#[test]
fn reconcile_active_tasks_adopts_owned_in_progress_task_from_worktree_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "replay-owned-task");
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(&repo, vec![manager, engineer]);
    daemon.set_member_idle("eng-1");

    write_board_task_file(
        &repo,
        10,
        "active-task",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-main/eng-1", &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/10").unwrap();
    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    git_ok(&worktree_dir, &["add", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "task work"]);

    daemon.reconcile_active_tasks().unwrap();
    assert_eq!(daemon.active_task_id("eng-1"), Some(10));
}

#[test]
fn reconcile_active_tasks_removes_transient_executor_worktree_after_reassignment() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "executor-reassignment-cleanup");
    let manager = manager_member("manager", None);
    let engineer = engineer_member("eng-1-1", Some("manager"), true);
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1-1".to_string(), MemberState::Idle)]))
        .build();

    let executor_worktree = repo.join(".batty").join("worktrees").join("executor-1");
    std::fs::create_dir_all(executor_worktree.parent().unwrap()).unwrap();
    git_ok(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "executor-1/681",
            executor_worktree.to_str().unwrap(),
            "main",
        ],
    );
    write_owned_task_file(&repo, 681, "reassigned-task", "in-progress", "eng-1-1");

    daemon.reconcile_active_tasks().unwrap();

    assert!(!daemon.active_tasks.contains_key("executor-1"));
    assert_eq!(daemon.active_task_id("eng-1-1"), Some(681));
    assert!(!executor_worktree.exists());
    assert!(
        !crate::team::git_cmd::show_ref_exists(&repo, "executor-1/681").unwrap(),
        "executor task branch should be removed after reassignment cleanup"
    );
    let worktrees = git_stdout(&repo, &["worktree", "list", "--porcelain"]);
    assert!(
        !worktrees.contains("executor-1"),
        "git worktree list should not retain executor worktree:\n{worktrees}"
    );
}

#[test]
fn reconcile_active_tasks_noop_when_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = make_test_daemon(tmp.path(), Vec::new());
    // No active tasks — should return immediately
    daemon.reconcile_active_tasks().unwrap();
    assert!(daemon.active_tasks.is_empty());
}

#[test]
fn reconcile_active_tasks_escalates_repeat_orphan_rescue_to_blocked() {
    // #714: when a task is orphan-rescued a 2nd time within the cascade
    // window, it's cycling through engineers who each release within
    // seconds. Escalate to `blocked` + notify manager so dispatch stops
    // auto-redispatching it. Mirrors batty-marketing 2026-04-16 23:09
    // (#523 bounced 4 engineers in 4 minutes) and 2026-04-17 12:22 (#572
    // bounced sam-designer → alex-dev in 6 minutes).
    let tmp = tempfile::tempdir().unwrap();
    let manager = manager_member("manager", None);
    let engineer = engineer_member("eng-1", Some("manager"), false);
    let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);

    write_board_task_file(
        tmp.path(),
        42,
        "cascade-task",
        "in-progress",
        None,
        &[],
        None,
    );

    // Seed a recent rescue record for task #42 so this reconcile pass
    // counts as the 2nd rescue within the cascade window.
    daemon.dispatch.recently_rescued_tasks.insert(
        42,
        crate::team::daemon::RescueRecord {
            last_rescued_at: Instant::now(),
            count: 1,
        },
    );

    daemon.reconcile_active_tasks().unwrap();

    let task_path = board_task_path(tmp.path(), 42);
    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(
        task.status, "blocked",
        "repeat orphan-rescue must escalate to blocked, not todo"
    );
    assert!(
        task.blocked.is_some(),
        "block_reason must be set so the task surfaces in triage"
    );
    assert!(
        task.blocked
            .as_deref()
            .unwrap_or("")
            .contains("orphan-rescue cascade"),
        "block_reason should name the cascade so manager knows why it escalated (got {:?})",
        task.blocked
    );

    let inbox_root = inbox::inboxes_root(tmp.path());
    let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(
        manager_messages
            .iter()
            .any(|message| message.body.contains("Task #42")
                && message.body.contains("orphan-rescued repeatedly")),
        "manager must be notified of the cascade escalation (got {} messages)",
        manager_messages.len()
    );
}

#[test]
fn reconcile_active_tasks_first_orphan_rescue_moves_to_todo() {
    // #714 regression-guard: the escalation path must only trigger on
    // repeat rescues — the first rescue still moves the task back to
    // todo so normal dispatch can pick it up. Without this guard, any
    // single engineer release would block the task for manager triage,
    // which is heavier than the actual problem.
    let tmp = tempfile::tempdir().unwrap();
    let manager = manager_member("manager", None);
    let engineer = engineer_member("eng-1", Some("manager"), false);
    let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);

    write_board_task_file(
        tmp.path(),
        99,
        "fresh-orphan",
        "in-progress",
        None,
        &[],
        None,
    );

    // recently_rescued_tasks intentionally empty — this is the first
    // rescue and should fall through to the normal todo transition.
    daemon.reconcile_active_tasks().unwrap();

    let task_path = board_task_path(tmp.path(), 99);
    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(
        task.status, "todo",
        "first orphan-rescue must keep existing behavior (move to todo)"
    );
    assert!(
        task.blocked.is_none(),
        "first rescue must not set block_reason"
    );

    let inbox_root = inbox::inboxes_root(tmp.path());
    let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(
        manager_messages
            .iter()
            .all(|message| !message.body.contains("orphan-rescued repeatedly")),
        "manager must not be notified on a single rescue"
    );

    assert!(
        daemon.dispatch.recently_rescued_tasks.contains_key(&99),
        "first rescue must record cooldown so a repeat escalates correctly"
    );
}

#[test]
fn select_authoritative_multi_claim_task_prefers_priority_then_claim_age() {
    let mut older_high = test_task(10, "in-progress", Some("eng-1"), None);
    older_high.priority = "high".to_string();
    older_high.claimed_at = Some("2026-04-14T21:06:20Z".to_string());

    let mut newer_critical = test_task(20, "in-progress", Some("eng-1"), None);
    newer_critical.priority = "critical".to_string();
    newer_critical.claimed_at = Some("2026-04-14T21:06:30Z".to_string());

    assert_eq!(
        select_authoritative_multi_claim_task(&[&older_high, &newer_critical]).map(|task| task.id),
        Some(20),
        "higher priority must win even when claimed later"
    );

    let mut older_critical = test_task(30, "in-progress", Some("eng-1"), None);
    older_critical.priority = "critical".to_string();
    older_critical.claimed_at = Some("2026-04-14T21:06:10Z".to_string());

    let mut newer_critical_same_priority = test_task(40, "in-progress", Some("eng-1"), None);
    newer_critical_same_priority.priority = "critical".to_string();
    newer_critical_same_priority.claimed_at = Some("2026-04-14T21:06:40Z".to_string());

    assert_eq!(
        select_authoritative_multi_claim_task(&[&older_critical, &newer_critical_same_priority])
            .map(|task| task.id),
        Some(30),
        "oldest claim must win when priorities tie"
    );
}

#[test]
fn reconcile_active_tasks_repairs_multiple_claimed_tasks_for_one_engineer_in_one_pass() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);

    write_board_task_file(
        tmp.path(),
        10,
        "old-task",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );
    write_board_task_file(
        tmp.path(),
        20,
        "manual-override-task",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );

    let older_task_path = board_task_path(tmp.path(), 10);
    update_task_frontmatter(&older_task_path, |mapping| {
        mapping.insert(
            yaml_key("claimed_at"),
            serde_yaml::Value::String("2026-04-14T21:06:10Z".to_string()),
        );
    })
    .unwrap();

    let newer_task_path = board_task_path(tmp.path(), 20);
    update_task_frontmatter(&newer_task_path, |mapping| {
        mapping.insert(
            yaml_key("claimed_at"),
            serde_yaml::Value::String("2026-04-14T21:06:40Z".to_string()),
        );
    })
    .unwrap();

    daemon.reconcile_active_tasks().unwrap();

    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let tasks = crate::task::load_tasks_from_dir(&tasks_dir).unwrap();
    let eng_tasks: Vec<_> = tasks
        .iter()
        .filter(|task| task.claimed_by.as_deref() == Some("eng-1"))
        .map(|task| (task.id, task.status.as_str()))
        .collect();

    assert!(eng_tasks.contains(&(10, "in-progress")));
    assert!(!eng_tasks.contains(&(20, "in-progress")));
    assert_eq!(daemon.active_task_id("eng-1"), Some(10));

    let normalized = tasks.iter().find(|task| task.id == 20).unwrap();
    assert_eq!(normalized.status, "todo");
    assert!(normalized.claimed_by.is_none());

    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let first_pass_events = crate::team::events::read_events(&events_path).unwrap();
    let first_pass_repairs = first_pass_events
        .iter()
        .filter(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some("repair")
        })
        .count();
    assert_eq!(
        first_pass_repairs, 2,
        "first pass should emit one authoritative-task repair and one excess-claim release"
    );

    daemon.reconcile_active_tasks().unwrap();

    let second_pass_events = crate::team::events::read_events(&events_path).unwrap();
    let second_pass_repairs = second_pass_events
        .iter()
        .filter(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.reason.as_deref() == Some("repair")
        })
        .count();
    assert_eq!(
        second_pass_repairs, first_pass_repairs,
        "stable winner selection must converge without extra repair events on the next pass"
    );
    assert_eq!(daemon.active_task_id("eng-1"), Some(10));
}

#[test]
fn reconcile_active_tasks_adopts_board_task_after_clearing_stale_tracked_task() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
    daemon.active_tasks.insert("eng-1".to_string(), 10);

    write_board_task_file(
        tmp.path(),
        20,
        "replacement-task",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(20));
}

#[test]
fn claimed_task_branch_mismatch_reports_detached_head_claimed_task() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claimed-branch-detached-head");
    let engineer = engineer_member("eng-1", Some("manager"), true);
    let manager = manager_member("manager", None);
    let daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    git_ok(&worktree_dir, &["checkout", "--detach", &head]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    let mismatch = daemon
        .claimed_task_branch_mismatch("eng-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        mismatch,
        ClaimedTaskBranchMismatch {
            task_id: 42,
            expected_branch: "eng-1/42".to_string(),
            current_branch: "HEAD".to_string(),
        }
    );
}

#[test]
fn authoritative_task_branch_ignores_previous_assignees_branch() {
    let mut task = test_task(28, "in-progress", Some("eng-2"), None);
    task.branch = Some("eng-1/28".to_string());
    assert_eq!(authoritative_task_branch("eng-2", &task), "eng-2/28");
    task.branch = Some("eng-2/task-28-fix".to_string());
    assert_eq!(
        authoritative_task_branch("eng-2", &task),
        "eng-2/task-28-fix"
    );
    task.branch = None;
    assert_eq!(authoritative_task_branch("eng-2", &task), "eng-2/28");
}

#[test]
fn claimed_task_branch_mismatch_expects_own_branch_after_reassignment() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claimed-branch-reassigned");
    let daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-2", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-2".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-2");
    let base_branch = engineer_base_branch_name("eng-2");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    // Task was previously worked by eng-1; frontmatter still names its branch.
    write_owned_task_file_with_context(
        &repo,
        28,
        "reassigned-task",
        "in-progress",
        "eng-2",
        "eng-1/28",
        ".batty/worktrees/eng-1",
    );

    let mismatch = daemon
        .claimed_task_branch_mismatch("eng-2")
        .unwrap()
        .unwrap();
    assert_eq!(mismatch.expected_branch, "eng-2/28");
    assert_eq!(mismatch.current_branch, base_branch);
}

#[test]
fn claimed_task_branch_mismatch_ignores_matching_claimed_branch() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claimed-branch-match");
    let engineer = engineer_member("eng-1", Some("manager"), true);
    let manager = manager_member("manager", None);
    let daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    assert!(
        daemon
            .claimed_task_branch_mismatch("eng-1")
            .unwrap()
            .is_none()
    );
}

#[test]
fn claimed_task_branch_mismatch_ignores_detached_head_without_claim() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claimed-branch-no-claim");
    let engineer = engineer_member("eng-1", Some("manager"), true);
    let manager = manager_member("manager", None);
    let daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    git_ok(&worktree_dir, &["checkout", "--detach", &head]);

    assert!(
        daemon
            .claimed_task_branch_mismatch("eng-1")
            .unwrap()
            .is_none()
    );
}

#[test]
fn reconcile_active_tasks_repairs_clean_branch_mismatch_to_expected_branch() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-clean-branch-repair");
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("42")
            && event.reason.as_deref() == Some("branch_repair")
    }));
    let inbox_root = inbox::inboxes_root(&repo);
    assert!(
        inbox::pending_messages(&inbox_root, "manager")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reconcile_active_tasks_preserves_dirty_work_then_repairs_branch_mismatch() {
    // Regression: previously, a dirty worktree on the wrong branch would
    // cause the reconciliation path to refuse recovery and just alert.
    // This left engineers permanently stuck on stale branches until a
    // human intervened. The fix: auto-save dirty changes as a commit on
    // the current (stale) branch first, then switch to the expected
    // branch. Dirty work is preserved in git history and the engineer
    // lands on the correct lane.
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-stale-branch");
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
    std::fs::write(worktree_dir.join("scratch.txt"), "stale dirty work\n").unwrap();
    git_ok(&worktree_dir, &["add", "scratch.txt"]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    daemon.reconcile_active_tasks().unwrap();

    // Task ownership recorded.
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    // Branch was switched to the expected lane.
    assert_eq!(
        current_worktree_branch(&worktree_dir).unwrap(),
        "eng-1/42",
        "branch should have been recovered to the expected lane"
    );
    // Dirty file is now committed (not dirty, not lost). Use the same
    // semantics as production (ignores .batty/ and .cargo/ which are
    // always excluded from preservation).
    assert!(
        !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
        "worktree should have no user changes after preserve-and-recover",
    );
    // Verify the auto-save commit exists on eng-1/41 (the original branch).
    let log_on_41 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/41", "-1"]);
    assert!(
        log_on_41.contains("auto-save before branch recovery"),
        "eng-1/41 tip should be the auto-save commit, was:\n{log_on_41}",
    );
    // The originally-dirty scratch.txt should exist on eng-1/41 as a
    // committed file, not in the current worktree (eng-1/42 doesn't have it).
    let files_on_41 = git_stdout(&worktree_dir, &["ls-tree", "-r", "eng-1/41", "--name-only"]);
    assert!(
        files_on_41.contains("scratch.txt"),
        "scratch.txt should be committed on eng-1/41, files:\n{files_on_41}",
    );
    // Reconciliation event recorded as a branch_repair, not a mismatch
    // block.
    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(
        events.iter().any(|event| {
            event.event == "state_reconciliation"
                && event.role.as_deref() == Some("eng-1")
                && event.task.as_deref() == Some("42")
                && event.reason.as_deref() == Some("branch_repair")
        }),
        "expected a branch_repair state_reconciliation event, got: {:?}",
        events
            .iter()
            .filter(|e| e.event == "state_reconciliation")
            .collect::<Vec<_>>(),
    );
}

#[test]
fn reconcile_active_tasks_self_heals_working_engineer_on_stale_branch() {
    // Regression for #666: a Working engineer stuck on the wrong branch
    // with a dirty worktree used to be skipped by the self-heal path
    // (which only ran for Idle members). That left engineers wedged
    // re-emitting "branch recovery blocked" every monitor tick with no
    // healing action. The fix: self-heal runs for Working members too,
    // auto-saving dirty work on the stale branch before switching.
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-stale-branch-working");
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        // The key difference from the existing regression test: Working, not Idle.
        .states(HashMap::from([("eng-1".to_string(), MemberState::Working)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
    std::fs::write(worktree_dir.join("scratch.txt"), "stale working work\n").unwrap();
    git_ok(&worktree_dir, &["add", "scratch.txt"]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    daemon.reconcile_active_tasks().unwrap();

    // Branch recovery completed despite Working state.
    assert_eq!(
        current_worktree_branch(&worktree_dir).unwrap(),
        "eng-1/42",
        "branch should have been recovered even while Working"
    );
    // Dirty work preserved as a commit on the stale branch.
    let log_on_41 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/41", "-1"]);
    assert!(
        log_on_41.contains("auto-save before branch recovery"),
        "eng-1/41 tip should be the auto-save commit while Working, was:\n{log_on_41}",
    );
}

#[test]
fn reconcile_active_tasks_ignores_gitignored_runtime_noise() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-runtime-noise");
    std::fs::write(repo.join(".gitignore"), ".batty-target/\n").unwrap();
    git_ok(&repo, &["add", ".gitignore"]);
    git_ok(&repo, &["commit", "-m", "ignore runtime target"]);
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
    std::fs::create_dir_all(worktree_dir.join(".batty-target").join("debug")).unwrap();
    std::fs::write(
        worktree_dir
            .join(".batty-target")
            .join("debug")
            .join("build.log"),
        "transient\n",
    )
    .unwrap();

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
    assert!(
        !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
        "runtime-only .batty-target noise should not survive reconciliation as user dirt"
    );
    let inbox_root = inbox::inboxes_root(&repo);
    assert!(
        inbox::pending_messages(&inbox_root, "manager")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reconcile_active_tasks_requeues_orphaned_branch_mismatch_after_retry_limit() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-orphan-branch-mismatch");
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/108"]);
    std::fs::write(worktree_dir.join("scratch.txt"), "stale dirty work\n").unwrap();
    git_ok(&worktree_dir, &["add", "scratch.txt"]);
    let git_dir = PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        worktree_dir.join(git_dir)
    };
    std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

    write_owned_task_file_with_context(
        &repo,
        124,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/124",
        ".batty/worktrees/eng-1",
    );

    let _max_attempts = EnvVarGuard::set("BATTY_ORPHAN_BRANCH_MISMATCH_MAX_ATTEMPTS", "1");
    daemon.reconcile_active_tasks().unwrap();

    let task = crate::task::load_task_by_id(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
        124,
    )
    .unwrap();
    assert_eq!(task.status, "todo");
    assert!(task.claimed_by.is_none());
    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("124")
            && event.reason.as_deref() == Some("orphan_branch_mismatch_requeued")
    }));
}

#[test]
fn reconcile_active_tasks_blocks_detached_head_branch_mismatch() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "reconcile-detached-branch");
    let manager = manager_member("manager", None);
    let engineer = engineer_member("eng-1", Some("manager"), true);
    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![manager, engineer])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    let head = crate::team::test_support::git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    git_ok(&worktree_dir, &["checkout", "--detach", &head]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    daemon.reconcile_active_tasks().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "HEAD");
    let inbox_root = inbox::inboxes_root(&repo);
    let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(manager_messages.iter().any(|message| {
        message.body.contains("Reconciliation alert")
            && message.body.contains("detached HEAD state")
    }));
}

#[test]
fn deliver_automation_nudge_suppresses_engineer_nudge_when_branch_mismatch_exists() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "nudge-branch-mismatch");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");

    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/41"]);
    write_owned_task_file_with_context(
        &repo,
        42,
        "authoritative-task",
        "in-progress",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    let delivery = daemon
        .deliver_automation_nudge("eng-1", "Idle nudge: move the task forward.")
        .unwrap();

    assert_eq!(delivery, MessageDelivery::OrchestratorLogged);
    let inbox_root = inbox::inboxes_root(&repo);
    let engineer_messages = inbox::pending_messages(&inbox_root, "eng-1").unwrap();
    assert!(
        engineer_messages
            .iter()
            .all(|message| { !message.body.contains("Idle nudge: move the task forward.") })
    );
    let manager_messages = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(manager_messages.iter().any(|message| {
        message.body.contains("Reconciliation alert")
            && message.body.contains("eng-1/41")
            && message.body.contains("eng-1/42")
    }));
}

// ── manager_for_member_name ──────────────────────────────────

#[test]
fn manager_for_member_name_returns_reports_to() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let daemon = make_test_daemon(tmp.path(), vec![manager, engineer]);
    assert_eq!(daemon.manager_for_member_name("eng-1"), Some("manager"));
}

#[test]
fn manager_for_member_name_returns_none_for_top_level() {
    let tmp = tempfile::tempdir().unwrap();
    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let daemon = make_test_daemon(tmp.path(), vec![architect]);
    assert_eq!(daemon.manager_for_member_name("architect"), None);
}

#[test]
fn manager_for_member_name_returns_none_for_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = make_test_daemon(tmp.path(), Vec::new());
    assert_eq!(daemon.manager_for_member_name("nobody"), None);
}

// ── auto_unblock_notification_recipient ──────────────────────

#[test]
fn auto_unblock_recipient_is_task_owner_when_known() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let daemon = make_test_daemon(tmp.path(), vec![engineer]);

    let task = crate::task::Task {
        id: 10,
        title: "test".to_string(),
        status: "blocked".to_string(),
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
        tags: Vec::new(),
        depends_on: vec![1],
        review_owner: None,
        blocked_on: None,
        worktree_path: None,
        branch: None,
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: std::path::PathBuf::new(),
    };
    assert_eq!(
        daemon.auto_unblock_notification_recipient(&task),
        Some("eng-1".to_string())
    );
}

#[test]
fn auto_unblock_recipient_falls_back_to_manager() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let daemon = make_test_daemon(tmp.path(), vec![manager]);

    let task = crate::task::Task {
        id: 10,
        title: "test".to_string(),
        status: "blocked".to_string(),
        priority: "high".to_string(),
        assignee: None,
        claimed_by: None,
        claimed_at: None,
        claim_ttl_secs: None,
        claim_expires_at: None,
        last_progress_at: None,
        claim_warning_sent_at: None,
        claim_extensions: None,
        last_output_bytes: None,
        blocked: None,
        tags: Vec::new(),
        depends_on: vec![1],
        review_owner: None,
        blocked_on: None,
        worktree_path: None,
        branch: None,
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: std::path::PathBuf::new(),
    };
    assert_eq!(
        daemon.auto_unblock_notification_recipient(&task),
        Some("manager".to_string())
    );
}

#[test]
fn auto_unblock_recipient_ignores_unknown_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let daemon = make_test_daemon(tmp.path(), vec![manager]);

    let task = crate::task::Task {
        id: 10,
        title: "test".to_string(),
        status: "blocked".to_string(),
        priority: "high".to_string(),
        assignee: None,
        claimed_by: Some("unknown-eng".to_string()),
        claimed_at: None,
        claim_ttl_secs: None,
        claim_expires_at: None,
        last_progress_at: None,
        claim_warning_sent_at: None,
        claim_extensions: None,
        last_output_bytes: None,
        blocked: None,
        tags: Vec::new(),
        depends_on: vec![1],
        review_owner: None,
        blocked_on: None,
        worktree_path: None,
        branch: None,
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: std::path::PathBuf::new(),
    };
    // Owner not in members → falls back to manager
    assert_eq!(
        daemon.auto_unblock_notification_recipient(&task),
        Some("manager".to_string())
    );
}

// ── truly_idle_engineer_count ────────────────────────────────

#[test]
fn truly_idle_counts_only_idle_engineers_without_board_items() {
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Idle);
    states.insert("eng-2".to_string(), MemberState::Idle);
    states.insert("eng-3".to_string(), MemberState::Working);

    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let eng2 = MemberInstance {
        name: "eng-2".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let eng3 = MemberInstance {
        name: "eng-3".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![eng1, eng2, eng3])
        .states(states)
        .build();

    // eng-3 is Working with an active task → not idle per idle_engineer_names
    daemon.active_tasks.insert("eng-3".to_string(), 99);

    // eng-2 has an in-progress task on the board
    let tasks = vec![crate::task::Task {
        id: 1,
        title: "active-task".to_string(),
        status: "in-progress".to_string(),
        priority: "high".to_string(),
        assignee: None,
        claimed_by: Some("eng-2".to_string()),
        claimed_at: None,
        claim_ttl_secs: None,
        claim_expires_at: None,
        last_progress_at: None,
        claim_warning_sent_at: None,
        claim_extensions: None,
        last_output_bytes: None,
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
        review_owner: None,
        blocked_on: None,
        worktree_path: None,
        branch: None,
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: std::path::PathBuf::new(),
    }];

    // eng-1 is idle with no board items → truly idle
    // eng-2 is idle but has in-progress task → not truly idle
    // eng-3 is working → not idle at all
    assert_eq!(daemon.truly_idle_engineer_count(&tasks), 1);
}

#[test]
fn truly_idle_count_is_zero_when_all_busy() {
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Working);

    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![eng1])
        .states(states)
        .build();

    // Working engineer needs an active task to not be treated as idle
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    assert_eq!(daemon.truly_idle_engineer_count(&[]), 0);
}

#[test]
fn truly_idle_strips_at_prefix_from_claimed_by() {
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Idle);

    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![eng1])
        .states(states)
        .build();

    let tasks = vec![crate::task::Task {
        id: 1,
        title: "task".to_string(),
        status: "todo".to_string(),
        priority: "high".to_string(),
        assignee: None,
        claimed_by: Some("@eng-1".to_string()),
        claimed_at: None,
        claim_ttl_secs: None,
        claim_expires_at: None,
        last_progress_at: None,
        claim_warning_sent_at: None,
        claim_extensions: None,
        last_output_bytes: None,
        blocked: None,
        tags: Vec::new(),
        depends_on: Vec::new(),
        review_owner: None,
        blocked_on: None,
        worktree_path: None,
        branch: None,
        commit: None,
        artifacts: Vec::new(),
        next_action: None,
        scheduled_for: None,
        cron_schedule: None,
        cron_last_run: None,
        completed: None,
        description: String::new(),
        batty_config: None,
        source_path: std::path::PathBuf::new(),
    }];

    // eng-1 has a todo task (with @ prefix) — not truly idle
    assert_eq!(daemon.truly_idle_engineer_count(&tasks), 0);
}

// ── maybe_escalate_stale_reviews ─────────────────────────────

#[test]
fn escalate_stale_reviews_sends_nudge_then_escalation() {
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "manager").unwrap();
    inbox::init_inbox(&inbox_root, "architect").unwrap();

    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
        ..Default::default()
    };

    // Use tiny thresholds for testing
    let policy = WorkflowPolicy {
        review_nudge_threshold_secs: 5,
        review_timeout_secs: 10,
        ..WorkflowPolicy::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect, manager])
        .workflow_policy(policy)
        .build();
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    // Write a task in review
    write_board_task_file(
        tmp.path(),
        50,
        "review-task",
        "review",
        Some("eng-1"),
        &[],
        None,
    );

    // First call: task just entered review, no nudge yet (age = 0)
    daemon.maybe_escalate_stale_reviews().unwrap();
    let pending_manager = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(pending_manager.is_empty(), "no nudge should fire at age 0");

    // Simulate the task having been first seen long enough ago for nudge
    daemon.interventions.review_first_seen.insert(50, 0); // epoch = 0, so age will be huge
    daemon.interventions.review_nudge_sent.clear();

    daemon.maybe_escalate_stale_reviews().unwrap();

    // At this point the age is >> both nudge (5s) and timeout (10s),
    // so escalation fires (escalation > nudge, and escalation check comes first)
    let pending_architect = inbox::pending_messages(&inbox_root, "architect").unwrap();
    assert!(
        pending_architect
            .iter()
            .any(|msg| msg.body.contains("Review timeout")),
        "architect should receive escalation message"
    );

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(
        events.iter().any(|e| e.event == "review_escalated"),
        "review_escalated event should be emitted"
    );
}

#[test]
fn escalate_stale_reviews_sends_nudge_below_timeout() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;
    use std::time::{SystemTime, UNIX_EPOCH};

    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "manager").unwrap();

    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let policy = WorkflowPolicy {
        review_nudge_threshold_secs: 5,
        review_timeout_secs: 999_999, // very high so escalation won't fire
        ..WorkflowPolicy::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager])
        .workflow_policy(policy)
        .build();
    daemon.config.team_config.workflow_mode = crate::team::config::WorkflowMode::Hybrid;
    daemon.config.team_config.orchestrator_pane = true;
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    write_board_task_file(tmp.path(), 60, "nudge-task", "review", None, &[], None);

    // Simulate first_seen long enough ago to trigger nudge but not timeout
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    daemon.interventions.review_first_seen.insert(60, now - 100);

    daemon.maybe_escalate_stale_reviews().unwrap();

    let pending_manager = inbox::pending_messages(&inbox_root, "manager").unwrap();
    let orchestrator_log =
        std::fs::read_to_string(crate::team::orchestrator_log_path(tmp.path())).unwrap();
    assert!(
        pending_manager.is_empty(),
        "review nudge should stay out of inbox"
    );
    assert!(
        orchestrator_log.contains("Review nudge"),
        "review nudge should be recorded in orchestrator log"
    );
    assert!(daemon.interventions.review_nudge_sent.contains(&60));

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(events.iter().any(|e| e.event == "review_nudge_sent"));
}

#[test]
fn escalate_stale_reviews_skips_non_review_tasks() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "manager").unwrap();

    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let policy = WorkflowPolicy {
        review_nudge_threshold_secs: 1,
        review_timeout_secs: 2,
        ..WorkflowPolicy::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager])
        .workflow_policy(policy)
        .build();

    // Only in-progress and todo tasks — no review tasks
    write_board_task_file(tmp.path(), 70, "ip-task", "in-progress", None, &[], None);
    write_board_task_file(tmp.path(), 71, "todo-task", "todo", None, &[], None);

    daemon.maybe_escalate_stale_reviews().unwrap();

    let pending = inbox::pending_messages(&inbox_root, "manager").unwrap();
    assert!(pending.is_empty());
}

#[test]
fn escalate_stale_reviews_prunes_tracking_for_non_review_tasks() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![manager])
        .workflow_policy(WorkflowPolicy::default())
        .build();

    // Pre-populate tracking with task IDs that are no longer in review
    daemon.interventions.review_first_seen.insert(80, 1000);
    daemon.interventions.review_first_seen.insert(81, 2000);
    daemon.interventions.review_nudge_sent.insert(80);

    // Only task 80 exists and it's done, 81 doesn't exist at all
    write_board_task_file(tmp.path(), 80, "done-task", "done", None, &[], None);

    daemon.maybe_escalate_stale_reviews().unwrap();

    assert!(!daemon.interventions.review_first_seen.contains_key(&80));
    assert!(!daemon.interventions.review_first_seen.contains_key(&81));
    assert!(!daemon.interventions.review_nudge_sent.contains(&80));
}

#[test]
fn stale_review_tracking_emits_state_reconciliation_event() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let mut daemon = make_test_daemon(tmp.path(), Vec::new());
    std::fs::create_dir_all(
        tmp.path()
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )
    .unwrap();
    daemon.event_sink = EventSink::new(&events_path).unwrap();
    daemon.interventions.review_first_seen.insert(80, 1000);
    daemon.interventions.review_nudge_sent.insert(80);

    daemon.maybe_escalate_stale_reviews().unwrap();

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.task.as_deref() == Some("80")
            && event.reason.as_deref() == Some("review_fix")
    }));
}

#[test]
fn missing_review_tracking_emits_state_reconciliation_event() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    let mut daemon = make_test_daemon(tmp.path(), Vec::new());
    daemon.event_sink = EventSink::new(&events_path).unwrap();
    write_board_task_file(
        tmp.path(),
        60,
        "review-task",
        "review",
        Some("eng-1"),
        &[],
        None,
    );

    daemon.maybe_escalate_stale_reviews().unwrap();

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.task.as_deref() == Some("60")
            && event.reason.as_deref() == Some("review_fix")
    }));
}

#[test]
fn active_stale_review_entries_skip_tasks_that_left_review() {
    let task = test_task(90, "done", Some("eng-1"), Some("manager"));
    let tasks_by_id = HashMap::from([(task.id, &task)]);
    let report = crate::team::board::TaskAgingReport {
        stale_in_progress: Vec::new(),
        aged_todo: Vec::new(),
        stale_review: vec![crate::team::board::AgedTask {
            task_id: 90,
            title: "task-90".to_string(),
            status: "review".to_string(),
            claimed_by: Some("eng-1".to_string()),
            age_secs: 7_200,
        }],
    };

    let (active, suppressed) = active_stale_review_entries(&report, &tasks_by_id);

    assert!(active.is_empty());
    assert_eq!(suppressed, vec![90]);
}

// ── maybe_rotate_board ───────────────────────────────────────

#[test]
fn maybe_rotate_board_skips_when_board_dir_exists() {
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let board_dir = tmp.path().join(".batty").join("team_config").join("board");
    std::fs::create_dir_all(&board_dir).unwrap();

    let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
    // Force last rotation far in the past to trigger the check
    daemon.scheduler.last_board_rotation =
        std::time::Instant::now() - std::time::Duration::from_secs(700);

    daemon.maybe_rotate_board().unwrap();
    // No crash, no rotation needed for kanban-md directory board
}

#[test]
fn maybe_rotate_board_skips_when_too_recent() {
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
    // last_board_rotation is now (set by builder) — should skip
    daemon.maybe_rotate_board().unwrap();
    // No crash, just a no-op early return
}

// ── member_worktree_context ──────────────────────────────────

#[test]
fn member_worktree_context_returns_none_for_non_worktree_member() {
    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let daemon = make_test_daemon(tmp.path(), vec![engineer]);
    assert!(daemon.member_worktree_context("eng-1").is_none());
}

#[test]
fn member_worktree_context_returns_none_when_worktree_missing() {
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: true,
        ..Default::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![engineer])
        .build();
    daemon.is_git_repo = true;

    // Worktree directory doesn't exist
    assert!(daemon.member_worktree_context("eng-1").is_none());
}

// ── maybe_detect_pipeline_starvation ─────────────────────────

#[test]
fn pipeline_starvation_skipped_when_threshold_is_none() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: None,
        ..WorkflowPolicy::default()
    };

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .workflow_policy(policy)
        .build();

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(!daemon.scheduler.pipeline_starvation_fired);
}

#[test]
fn pipeline_starvation_skipped_when_no_idle_engineers() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: Some(1),
        ..WorkflowPolicy::default()
    };

    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };

    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Working);

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![eng1])
        .states(states)
        .workflow_policy(policy)
        .build();

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(!daemon.scheduler.pipeline_starvation_fired);
}

#[test]
fn pipeline_starvation_fires_when_deficit_exceeds_threshold() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "architect").unwrap();

    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: Some(1),
        ..WorkflowPolicy::default()
    };

    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
        ..Default::default()
    };

    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Idle);

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect, eng1])
        .states(states)
        .workflow_policy(policy)
        .build();

    // No todo tasks at all, 1 idle engineer → deficit = 1 >= threshold 1
    daemon.maybe_detect_pipeline_starvation().unwrap();

    assert!(daemon.scheduler.pipeline_starvation_fired);
    let pending = inbox::pending_messages(&inbox_root, "architect").unwrap();
    assert!(
        pending
            .iter()
            .any(|msg| msg.body.contains("Pipeline running dry")),
        "architect should be notified"
    );
}

#[test]
fn pipeline_starvation_suppressed_when_enough_todo_tasks() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::standup::MemberState;
    use crate::team::test_support::{TestDaemonBuilder, write_open_task_file};

    let tmp = tempfile::tempdir().unwrap();
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "architect").unwrap();

    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: Some(1),
        ..WorkflowPolicy::default()
    };

    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
        ..Default::default()
    };

    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Idle);

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect, eng1])
        .states(states)
        .workflow_policy(policy)
        .build();

    // 1 unclaimed todo task >= 1 idle engineer → no starvation
    write_open_task_file(tmp.path(), 90, "available-task", "todo");

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(!daemon.scheduler.pipeline_starvation_fired);
}

#[test]
fn pipeline_starvation_suppressed_when_manager_working() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();

    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: Some(1),
        ..WorkflowPolicy::default()
    };

    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };

    let mut states = std::collections::HashMap::new();
    states.insert("eng-1".to_string(), MemberState::Idle);
    states.insert("manager".to_string(), MemberState::Working);

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect, manager, eng1])
        .states(states)
        .workflow_policy(policy)
        .build();

    daemon.maybe_detect_pipeline_starvation().unwrap();
    assert!(
        !daemon.scheduler.pipeline_starvation_fired,
        "should suppress when manager is working"
    );
}

#[test]
fn auto_retro_does_not_fire_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let events_path = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("events.jsonl");
    write_owned_task_file(tmp.path(), 45, "retro-task", "done", "eng-1");
    write_event_log(
        tmp.path(),
        &[
            TeamEvent::daemon_started(),
            TeamEvent::task_assigned("eng-1", "45"),
            TeamEvent::task_completed("eng-1", Some("45")),
            TeamEvent::daemon_stopped(),
        ],
    );

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .orchestrator_pane(false)
        .build();
    daemon.event_sink = EventSink::new(&events_path).unwrap();

    daemon.maybe_generate_retrospective().unwrap();
    daemon.maybe_generate_retrospective().unwrap();

    let retro_dir = tmp.path().join(".batty").join("retrospectives");
    let reports = std::fs::read_dir(&retro_dir).unwrap().count();
    assert_eq!(reports, 1);

    let events = crate::team::events::read_events(&events_path).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event == "retro_generated")
            .count(),
        1
    );
}

// ── maybe_auto_archive ───────────────────────────────────────────

/// Helper: write a done task with a specific completed date (RFC3339).
fn write_done_task_with_completed(project_root: &Path, id: u32, title: &str, completed: &str) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let content = format!(
        "---\nid: {id}\ntitle: {title}\nstatus: done\npriority: high\ncompleted: \"{completed}\"\nclass: standard\n---\n\nTask.\n"
    );
    std::fs::write(tasks_dir.join(format!("{id:03}-{title}.md")), content).unwrap();
}

/// Backdate the rate-limit timer so the archive check fires immediately.
fn backdate_auto_archive(daemon: &mut TeamDaemon) {
    daemon.scheduler.last_auto_archive = Instant::now() - Duration::from_secs(120);
}

#[test]
fn auto_archive_moves_old_done_tasks() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let policy = WorkflowPolicy {
        auto_archive_done_after_secs: Some(60),
        ..WorkflowPolicy::default()
    };
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .workflow_policy(policy)
        .build();
    backdate_auto_archive(&mut daemon);

    // A task completed 2 hours ago — should be archived
    write_done_task_with_completed(tmp.path(), 1, "old-done", "2020-01-01T00:00:00+00:00");

    daemon.maybe_auto_archive().unwrap();

    let archive_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("archive");
    assert!(archive_dir.join("001-old-done.md").exists());

    let events =
        crate::team::events::read_events(&crate::team::team_events_path(tmp.path())).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "board_task_archived" && event.task.as_deref() == Some("1")
    }));
}

#[test]
fn auto_archive_skips_recent_done() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let policy = WorkflowPolicy {
        auto_archive_done_after_secs: Some(86400), // 24h
        ..WorkflowPolicy::default()
    };
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .workflow_policy(policy)
        .build();
    backdate_auto_archive(&mut daemon);

    // A task completed just now — should NOT be archived
    let now = chrono::Utc::now().to_rfc3339();
    write_done_task_with_completed(tmp.path(), 2, "recent-done", &now);

    daemon.maybe_auto_archive().unwrap();

    let archive_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("archive");
    assert!(!archive_dir.exists() || !archive_dir.join("002-recent-done.md").exists());
}

#[test]
fn auto_archive_respects_config_threshold() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    // Very large threshold — nothing should be archived
    let policy = WorkflowPolicy {
        auto_archive_done_after_secs: Some(999_999_999),
        ..WorkflowPolicy::default()
    };
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .workflow_policy(policy)
        .build();
    backdate_auto_archive(&mut daemon);

    // Even an old task shouldn't be archived with a huge threshold
    write_done_task_with_completed(tmp.path(), 3, "old-but-kept", "2024-01-01T00:00:00+00:00");

    daemon.maybe_auto_archive().unwrap();

    // Task file should still be in tasks/
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    assert!(tasks_dir.join("003-old-but-kept.md").exists());
}

#[test]
fn auto_archive_noop_when_disabled() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    // Disabled: auto_archive_done_after_secs = Some(0)
    let policy = WorkflowPolicy {
        auto_archive_done_after_secs: Some(0),
        ..WorkflowPolicy::default()
    };
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .workflow_policy(policy)
        .build();
    backdate_auto_archive(&mut daemon);

    write_done_task_with_completed(
        tmp.path(),
        4,
        "disabled-archive",
        "2020-01-01T00:00:00+00:00",
    );

    daemon.maybe_auto_archive().unwrap();

    // Task should remain in tasks/
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    assert!(tasks_dir.join("004-disabled-archive.md").exists());
}

#[test]
fn auto_archive_noop_when_none() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    // Disabled: auto_archive_done_after_secs = None (default)
    let policy = WorkflowPolicy {
        auto_archive_done_after_secs: None,
        ..WorkflowPolicy::default()
    };
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .workflow_policy(policy)
        .build();
    backdate_auto_archive(&mut daemon);

    write_done_task_with_completed(tmp.path(), 5, "none-archive", "2020-01-01T00:00:00+00:00");

    daemon.maybe_auto_archive().unwrap();

    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    assert!(tasks_dir.join("005-none-archive.md").exists());
}

// ── pipeline starvation time-bounded manager suppression ──

#[test]
fn pipeline_starvation_fires_when_manager_working_too_long() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "architect").unwrap();

    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: Some(1),
        ..WorkflowPolicy::default()
    };

    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };

    let mut states = std::collections::HashMap::new();
    states.insert("manager".to_string(), MemberState::Working);
    states.insert("eng-1".to_string(), MemberState::Idle);

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect, manager, eng1])
        .states(states)
        .workflow_policy(policy)
        .build();

    // Manager has no shim handle (non-shim mode) and no idle_started_at
    // entry, meaning the old code would suppress starvation. But since
    // there's no shim handle, the fallback checks idle_started_at. Without
    // an entry, it falls back to suppressed. To test the shim path:
    // Insert a mock shim handle for the manager that's been working for
    // 20 minutes (past the 10-minute grace).
    let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
    let channel = crate::shim::protocol::Channel::new(parent);
    let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
        "manager".into(),
        channel,
        999,
        "claude".into(),
        "claude".into(),
        std::path::PathBuf::from("/tmp/test"),
    );
    handle.apply_state_change(crate::shim::protocol::ShimState::Working);
    // Backdate to 20 minutes ago
    handle.state_changed_at = std::time::Instant::now() - std::time::Duration::from_secs(1200);
    daemon.shim_handles.insert("manager".to_string(), handle);

    daemon.maybe_detect_pipeline_starvation().unwrap();

    assert!(
        daemon.scheduler.pipeline_starvation_fired,
        "starvation should fire when manager has been working for >10 minutes"
    );
}

#[test]
fn pipeline_starvation_suppressed_when_manager_recently_working() {
    use crate::team::config::WorkflowPolicy;
    use crate::team::standup::MemberState;
    use crate::team::test_support::TestDaemonBuilder;

    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp
        .path()
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let inbox_root = inbox::inboxes_root(tmp.path());
    inbox::init_inbox(&inbox_root, "architect").unwrap();

    let policy = WorkflowPolicy {
        pipeline_starvation_threshold: Some(1),
        ..WorkflowPolicy::default()
    };

    let architect = MemberInstance {
        name: "architect".to_string(),
        role_name: "architect".to_string(),
        role_type: RoleType::Architect,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: Some("architect".to_string()),
        use_worktrees: false,
        ..Default::default()
    };
    let eng1 = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng".to_string(),
        role_type: RoleType::Engineer,
        agent: Some("codex".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: false,
        ..Default::default()
    };

    let mut states = std::collections::HashMap::new();
    states.insert("manager".to_string(), MemberState::Working);
    states.insert("eng-1".to_string(), MemberState::Idle);

    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(vec![architect, manager, eng1])
        .states(states)
        .workflow_policy(policy)
        .build();

    // Insert a mock shim handle for manager that's been working for only
    // 2 minutes (within the 10-minute grace).
    let (parent, _child) = crate::shim::protocol::socketpair().unwrap();
    let channel = crate::shim::protocol::Channel::new(parent);
    let mut handle = crate::team::daemon::agent_handle::AgentHandle::new(
        "manager".into(),
        channel,
        999,
        "claude".into(),
        "claude".into(),
        std::path::PathBuf::from("/tmp/test"),
    );
    handle.apply_state_change(crate::shim::protocol::ShimState::Working);
    // Just started working — within grace period
    daemon.shim_handles.insert("manager".to_string(), handle);

    daemon.maybe_detect_pipeline_starvation().unwrap();

    assert!(
        !daemon.scheduler.pipeline_starvation_fired,
        "starvation should be suppressed when manager is recently working"
    );
}

#[test]
fn claim_ttl_initializes_tracking_for_claimed_in_progress_task() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-ttl-init");
    write_owned_task_file(&repo, 42, "ttl-init", "in-progress", "eng-1");

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .build();

    daemon.maybe_manage_task_claim_ttls().unwrap();

    let task = crate::task::Task::from_file(&board_task_path(&repo, 42)).unwrap();
    assert_eq!(task.claim_ttl_secs, Some(900));
    assert!(task.claimed_at.is_some());
    assert!(task.claim_expires_at.is_some());
    assert!(task.last_progress_at.is_some());

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_claim_created" && event.task.as_deref() == Some("42")
    }));
}

#[test]
fn fresh_claim_grace_suppresses_stale_aging_with_old_progress_timestamp() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "fresh-claim-grace");
    write_board_task_file(
        &repo,
        42,
        "fresh-claim-grace",
        "in-progress",
        Some("eng-1"),
        &[],
        None,
    );

    let task_path = board_task_path(&repo, 42);
    let now = chrono::Utc::now();
    let old_started = (now - chrono::Duration::hours(8)).to_rfc3339();
    let stale_progress = (now - chrono::Duration::hours(7)).to_rfc3339();
    let fresh_claim = now.to_rfc3339();
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "started", Some(&old_started));
        set_optional_string(mapping, "updated", Some(&old_started));
        set_optional_string(mapping, "claimed_at", Some(&fresh_claim));
        set_optional_string(mapping, "last_progress_at", Some(&stale_progress));
        set_optional_u64(mapping, "last_output_bytes", Some(0));
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            architect_member("architect"),
            manager_member("manager", Some("architect")),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .workflow_policy(WorkflowPolicy {
            stale_in_progress_hours: 1,
            ..WorkflowPolicy::default()
        })
        .build();
    daemon
        .config
        .team_config
        .automation
        .intervention_cooldown_secs = 300;

    daemon.maybe_emit_task_aging_alerts().unwrap();

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(
        !events.iter().any(|event| {
            matches!(event.event.as_str(), "task_stale" | "task_escalated")
                && event.task.as_deref() == Some("42")
        }),
        "fresh claims should suppress stale aging even when last_progress_at is old"
    );
}

#[test]
fn claim_ttl_reclaims_expired_task_without_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-ttl-expire");
    write_owned_task_file(&repo, 42, "ttl-expire", "in-progress", "eng-1");

    let task_path = board_task_path(&repo, 42);
    let now = chrono::Utc::now();
    let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
    let recent_progress_time = now.to_rfc3339();
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_at", Some(&stale_time));
        set_optional_u64(mapping, "claim_ttl_secs", Some(60));
        set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
        set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
        set_optional_u64(mapping, "last_output_bytes", Some(0));
        mapping.insert(
            yaml_key("claim_extensions"),
            serde_yaml::Value::Number(0.into()),
        );
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.maybe_manage_task_claim_ttls().unwrap();

    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "todo");
    assert!(task.claimed_by.is_none());
    assert!(
        task.next_action
            .as_deref()
            .unwrap_or("")
            .contains("Reclaimed after TTL expiry")
    );
    assert_eq!(daemon.active_task_id("eng-1"), None);

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_claim_expired"
            && event.task.as_deref() == Some("42")
            && event
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("time_held_secs=")
    }));
}

#[test]
fn claim_ttl_reclaim_resets_engineer_worktree_to_base_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-ttl-reset");
    write_owned_task_file(&repo, 42, "ttl-reset", "in-progress", "eng-1");

    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/42"]);
    std::fs::write(worktree_dir.join("tracked.txt"), "tracked reclaim work\n").unwrap();
    git_ok(&worktree_dir, &["add", "tracked.txt"]);
    std::fs::write(
        worktree_dir.join("untracked.txt"),
        "untracked reclaim work\n",
    )
    .unwrap();

    let task_path = board_task_path(&repo, 42);
    let now = chrono::Utc::now();
    let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
    let recent_progress_time = now.to_rfc3339();
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_at", Some(&stale_time));
        set_optional_u64(mapping, "claim_ttl_secs", Some(60));
        set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
        set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
        set_optional_u64(mapping, "last_output_bytes", Some(0));
        mapping.insert(
            yaml_key("claim_extensions"),
            serde_yaml::Value::Number(0.into()),
        );
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.maybe_manage_task_claim_ttls().unwrap();

    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "todo");
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert_eq!(
        git_stdout(&repo, &["show", "eng-1/42:tracked.txt"]),
        "tracked reclaim work"
    );
    assert_eq!(
        git_stdout(&repo, &["show", "eng-1/42:untracked.txt"]),
        "untracked reclaim work"
    );
}

#[test]
fn claim_ttl_reclaim_blocks_lane_when_preserve_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-ttl-blocked");
    write_owned_task_file(&repo, 42, "ttl-blocked", "in-progress", "eng-1");

    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    git_ok(&worktree_dir, &["checkout", "-b", "eng-1/42"]);
    std::fs::write(worktree_dir.join("tracked.txt"), "tracked reclaim work\n").unwrap();
    git_ok(&worktree_dir, &["add", "tracked.txt"]);
    let git_dir = std::path::PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        worktree_dir.join(git_dir)
    };
    std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

    let task_path = board_task_path(&repo, 42);
    let now = chrono::Utc::now();
    let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
    let recent_progress_time = now.to_rfc3339();
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_at", Some(&stale_time));
        set_optional_u64(mapping, "claim_ttl_secs", Some(60));
        set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
        set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
        set_optional_u64(mapping, "last_output_bytes", Some(0));
        mapping.insert(
            yaml_key("claim_extensions"),
            serde_yaml::Value::Number(0.into()),
        );
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.maybe_manage_task_claim_ttls().unwrap();

    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "blocked");
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.iter().any(|message| {
        message
            .body
            .contains("could not safely auto-save eng-1's dirty worktree before TTL reclaim/reset")
    }));
}

#[test]
fn claim_ttl_reclaim_marks_same_engineer_as_recent_dispatch_and_prefers_alternative() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-ttl-redispatch");
    write_owned_task_file(&repo, 42, "ttl-redispatch", "in-progress", "eng-1");
    write_open_task_file(&repo, 99, "background-done", "done");

    let task_path = board_task_path(&repo, 42);
    let now = chrono::Utc::now();
    let stale_time = (now - chrono::Duration::minutes(40)).to_rfc3339();
    let recent_progress_time = now.to_rfc3339();
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_at", Some(&stale_time));
        set_optional_u64(mapping, "claim_ttl_secs", Some(60));
        set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
        set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
        set_optional_u64(mapping, "last_output_bytes", Some(0));
        mapping.insert(
            yaml_key("claim_extensions"),
            serde_yaml::Value::Number(0.into()),
        );
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
            engineer_member("eng-2", Some("manager"), false),
        ])
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            dispatch_dedup_window_secs: 60,
            ..BoardConfig::default()
        })
        .states(HashMap::from([
            ("eng-1".to_string(), MemberState::Idle),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 42);

    daemon.maybe_manage_task_claim_ttls().unwrap();

    assert!(
        daemon
            .dispatch
            .recent_dispatches
            .contains_key(&(42, "eng-1".to_string()))
    );

    daemon.scheduler.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-1".to_string(),
        Instant::now() - Duration::from_secs(60),
    );
    daemon.idle_started_at.insert(
        "eng-2".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.maybe_auto_dispatch().unwrap();

    assert_eq!(daemon.dispatch.queue.len(), 1);
    assert_eq!(daemon.dispatch.queue[0].task_id, 42);
    assert_eq!(daemon.dispatch.queue[0].engineer, "eng-2");
}

#[test]
fn automation_failed_tests_verification_retry_required_peer_pickup() {
    use crate::shim::protocol::{Command, socketpair};

    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "automation-failed-tests-retry-peer");
    write_owned_task_file(&repo, 42, "failed-tests-retry", "in-progress", "eng-1");
    write_owned_task_file(&repo, 777, "owner-other-work", "in-progress", "eng-1");
    mark_verification_retry_required(&repo, 42, "artifacts/retry-42.log");
    update_task_frontmatter(&board_task_path(&repo, 42), |mapping| {
        set_optional_string(mapping, "branch", Some("eng-1/42"));
        set_optional_string(mapping, "worktree_path", Some(".batty/worktrees/eng-1"));
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
            engineer_member("eng-2", Some("manager"), false),
        ])
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([
            ("eng-1".to_string(), MemberState::Working),
            ("eng-2".to_string(), MemberState::Idle),
        ]))
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 777);
    daemon.config.team_config.use_shim = true;
    let (parent_sock, child_sock) = socketpair().unwrap();
    let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
    let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
    let mut handle = super::super::agent_handle::AgentHandle::new(
        "eng-2".to_string(),
        parent_channel,
        12345,
        "codex".to_string(),
        "codex".to_string(),
        repo.clone(),
    );
    handle.apply_state_change(crate::shim::protocol::ShimState::Idle);
    daemon.shim_handles.insert("eng-2".to_string(), handle);
    daemon.scheduler.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-2".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.enqueue_dispatch_candidates().unwrap();

    assert_eq!(daemon.dispatch.queue.len(), 1);
    assert_eq!(daemon.dispatch.queue[0].task_id, 42);
    assert_eq!(daemon.dispatch.queue[0].engineer, "eng-2");

    daemon.process_dispatch_queue().unwrap();

    assert!(daemon.dispatch.queue.is_empty());
    assert_eq!(daemon.active_tasks.get("eng-2"), Some(&42));
    let task = crate::task::Task::from_file(&board_task_path(&repo, 42)).unwrap();
    assert_eq!(task.claimed_by.as_deref(), Some("eng-2"));
    assert_eq!(task.branch.as_deref(), Some("eng-1/42"));
    assert_eq!(
        task.worktree_path.as_deref(),
        Some(".batty/worktrees/eng-1")
    );
    let metadata = crate::team::board::read_workflow_metadata(&board_task_path(&repo, 42)).unwrap();
    assert_eq!(metadata.artifacts, vec!["artifacts/retry-42.log"]);

    let command = child_channel.recv().unwrap().unwrap();
    match command {
        Command::SendMessage { body, .. } => {
            assert!(body.contains("Previous owner: eng-1."));
            assert!(body.contains("Latest verification artifact: artifacts/retry-42.log."));
        }
        other => panic!("expected SendMessage, got {other:?}"),
    }
}

#[test]
fn automation_failed_tests_verification_retry_waits_while_normal_work_dispatchable() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "automation-failed-tests-retry-normal-work");
    write_owned_task_file(&repo, 42, "failed-tests-retry", "in-progress", "eng-1");
    write_owned_task_file(&repo, 777, "owner-other-work", "in-progress", "eng-1");
    write_open_task_file(&repo, 50, "normal-work", "todo");
    mark_verification_retry_required(&repo, 42, "artifacts/retry-42.log");

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
            engineer_member("eng-2", Some("manager"), false),
            engineer_member("eng-3", Some("manager"), false),
        ])
        .board(BoardConfig {
            auto_dispatch: true,
            dispatch_stabilization_delay_secs: 0,
            ..BoardConfig::default()
        })
        .states(HashMap::from([
            ("eng-1".to_string(), MemberState::Working),
            ("eng-2".to_string(), MemberState::Idle),
            ("eng-3".to_string(), MemberState::Idle),
        ]))
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 777);
    daemon.scheduler.last_auto_dispatch = Instant::now() - Duration::from_secs(30);
    daemon.idle_started_at.insert(
        "eng-2".to_string(),
        Instant::now() - Duration::from_secs(60),
    );
    daemon.idle_started_at.insert(
        "eng-3".to_string(),
        Instant::now() - Duration::from_secs(60),
    );

    daemon.enqueue_dispatch_candidates().unwrap();

    assert_eq!(daemon.dispatch.queue.len(), 1);
    assert_eq!(daemon.dispatch.queue[0].task_id, 50);
}

#[test]
fn claim_ttl_progress_from_output_extends_claim() {
    use crate::shim::protocol::socketpair;

    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "claim-ttl-output");
    write_owned_task_file(&repo, 42, "ttl-output", "in-progress", "eng-1");

    let task_path = board_task_path(&repo, 42);
    let now = chrono::Utc::now();
    let stale_time = (now - chrono::Duration::minutes(20)).to_rfc3339();
    let recent_progress_time = now.to_rfc3339();
    update_task_frontmatter(&task_path, |mapping| {
        set_optional_string(mapping, "claimed_at", Some(&stale_time));
        set_optional_u64(mapping, "claim_ttl_secs", Some(1800));
        set_optional_string(mapping, "claim_expires_at", Some(&stale_time));
        set_optional_string(mapping, "last_progress_at", Some(&recent_progress_time));
        set_optional_u64(mapping, "last_output_bytes", Some(0));
        mapping.insert(
            yaml_key("claim_extensions"),
            serde_yaml::Value::Number(0.into()),
        );
    })
    .unwrap();

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), false),
        ])
        .build();
    let (parent_sock, _child_sock) = socketpair().unwrap();
    let mut handle = super::super::agent_handle::AgentHandle::new(
        "eng-1".to_string(),
        crate::shim::protocol::Channel::new(parent_sock),
        12345,
        "codex".to_string(),
        "codex".to_string(),
        repo.to_path_buf(),
    );
    handle.record_output_bytes(128);
    daemon.shim_handles.insert("eng-1".to_string(), handle);

    daemon.maybe_manage_task_claim_ttls().unwrap();

    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.last_output_bytes, Some(128));
    assert!(task.claim_expires_at.as_deref().unwrap_or("") > stale_time.as_str());

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_claim_progress"
            && event.task.as_deref() == Some("42")
            && event.reason.as_deref() == Some("output")
    }));
}

#[test]
fn reconcile_stale_worktrees_rebases_clean_base_worktree_after_main_advances() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "worktree-auto-rebase");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();

    std::fs::write(repo.join("main.txt"), "advance main\n").unwrap();
    git_ok(&repo, &["add", "main.txt"]);
    git_ok(&repo, &["commit", "-m", "advance main"]);

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    daemon.maybe_reconcile_stale_worktrees().unwrap();

    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert_eq!(
        git_stdout(&repo, &["rev-parse", "main"]),
        git_stdout(&worktree_dir, &["rev-parse", "HEAD"])
    );

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "worktree_refreshed"
            && event.role.as_deref() == Some("eng-1")
            && event
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("rebased clean base worktree onto main")
    }));
}

#[test]
fn reconcile_stale_worktrees_skips_base_worktree_when_engineer_has_active_task() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "worktree-auto-rebase-active");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    let before_head = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);

    std::fs::write(repo.join("main.txt"), "advance main\n").unwrap();
    git_ok(&repo, &["add", "main.txt"]);
    git_ok(&repo, &["commit", "-m", "advance main"]);

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();
    daemon.active_tasks.insert("eng-1".to_string(), 488);

    daemon.maybe_reconcile_stale_worktrees().unwrap();

    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert_eq!(
        git_stdout(&worktree_dir, &["rev-parse", "HEAD"]),
        before_head
    );
}

#[test]
fn reconcile_stale_worktrees_resets_idle_branch_when_board_no_longer_authorizes_it() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "worktree-reset-unauthorized-branch");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    std::fs::write(worktree_dir.join("note.txt"), "stale branch work\n").unwrap();
    git_ok(&worktree_dir, &["add", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "stale branch work"]);

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    daemon.maybe_reconcile_stale_worktrees().unwrap();

    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "worktree_reconciled"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
    }));
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref() == Some("branch_reset")
    }));
}

#[test]
fn reconcile_stale_worktrees_preserves_staged_done_lane_before_safe_branch_reset() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "worktree-reset-done-staged");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    std::fs::write(worktree_dir.join("staged-only.txt"), "baseline\n").unwrap();
    git_ok(&worktree_dir, &["add", "staged-only.txt"]);
    git_ok(
        &worktree_dir,
        &["commit", "-m", "baseline stale branch file"],
    );
    std::fs::write(
        worktree_dir.join("staged-only.txt"),
        "staged dirty done work\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "staged-only.txt"]);

    write_owned_task_file_with_context(
        &repo,
        42,
        "done-task",
        "done",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    daemon.maybe_reconcile_stale_worktrees().unwrap();

    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert!(
        !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
        "worktree should be clean after preserving staged done-lane work"
    );
    let preserved_record =
        crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
    assert_eq!(
        preserved_record.artifact_kind,
        crate::team::checkpoint::PreservedLaneArtifactKind::Commit
    );
    assert_eq!(preserved_record.task_id, 42);
    assert_eq!(preserved_record.source_branch, "eng-1/42");
    let log_on_42 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/42", "-1"]);
    assert!(
        log_on_42.contains("preserve completed task #42 before branch recovery [eng-1/42]"),
        "expected preserve commit on stale branch, got:\n{log_on_42}"
    );
    let preserved = git_stdout(&worktree_dir, &["show", "eng-1/42:staged-only.txt"]);
    assert_eq!(preserved, "staged dirty done work");
    assert!(
        !worktree_dir.join("staged-only.txt").exists(),
        "reset to base branch should drop stale-branch-only file from current worktree"
    );

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "worktree_reconciled"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
    }));
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref() == Some("done_lane_reset")
    }));
}

#[test]
fn reconcile_stale_worktrees_preserves_unstaged_done_lane_before_safe_branch_reset() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "worktree-reset-done-unstaged");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    std::fs::write(
        worktree_dir.join("unstaged-only.txt"),
        "unstaged done work\n",
    )
    .unwrap();

    write_owned_task_file_with_context(
        &repo,
        42,
        "done-task",
        "done",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    daemon.maybe_reconcile_stale_worktrees().unwrap();

    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert!(
        !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
        "worktree should be clean after preserving unstaged done-lane work"
    );
    let preserved_record =
        crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
    assert_eq!(
        preserved_record.artifact_kind,
        crate::team::checkpoint::PreservedLaneArtifactKind::Commit
    );
    assert_eq!(preserved_record.task_id, 42);
    assert_eq!(preserved_record.source_branch, "eng-1/42");
    let log_on_42 = git_stdout(&worktree_dir, &["log", "--oneline", "eng-1/42", "-1"]);
    assert!(
        log_on_42.contains("preserve completed task #42 before branch recovery [eng-1/42]"),
        "expected preserve commit on stale branch, got:\n{log_on_42}"
    );
    let preserved = git_stdout(&worktree_dir, &["show", "eng-1/42:unstaged-only.txt"]);
    assert_eq!(preserved, "unstaged done work");
    assert!(
        !worktree_dir.join("unstaged-only.txt").exists(),
        "reset to base branch should drop stale-branch-only file from current worktree"
    );

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "worktree_reconciled"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
    }));
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref() == Some("done_lane_reset")
    }));
}

#[test]
fn reconcile_stale_worktrees_snapshots_dirty_done_lane_when_commit_preserve_fails() {
    let _path_lock = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "worktree-reset-done-preserve-failure");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    let team_config_dir = repo.join(".batty").join("team_config");
    let base_branch = engineer_base_branch_name("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, &base_branch, &team_config_dir).unwrap();
    crate::team::task_loop::checkout_worktree_branch_from_main(&worktree_dir, "eng-1/42").unwrap();
    std::fs::write(
        worktree_dir.join("blocked.txt"),
        "blocked dirty done work\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "blocked.txt"]);
    let git_dir = std::path::PathBuf::from(git_stdout(&worktree_dir, &["rev-parse", "--git-dir"]));
    let git_dir = if git_dir.is_absolute() {
        git_dir
    } else {
        worktree_dir.join(git_dir)
    };
    std::fs::write(git_dir.join("index.lock"), "locked\n").unwrap();

    write_owned_task_file_with_context(
        &repo,
        42,
        "done-task",
        "done",
        "eng-1",
        "eng-1/42",
        ".batty/worktrees/eng-1",
    );

    let mut daemon = TestDaemonBuilder::new(repo.as_path())
        .members(vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ])
        .states(HashMap::from([("eng-1".to_string(), MemberState::Idle)]))
        .build();

    daemon.maybe_reconcile_stale_worktrees().unwrap();

    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), base_branch);
    assert!(
        !crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap(),
        "worktree should be clean after snapshot fallback recreates it"
    );
    let task = crate::task::load_task_by_id(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
        42,
    )
    .unwrap();
    assert_eq!(task.status, "done");
    let preserved_record =
        crate::team::checkpoint::read_preserved_lane_record(&repo, "eng-1").unwrap();
    assert_eq!(
        preserved_record.artifact_kind,
        crate::team::checkpoint::PreservedLaneArtifactKind::Snapshot
    );
    let snapshot_rel = preserved_record.snapshot_path.clone().unwrap();
    let snapshot_abs = repo.join(&snapshot_rel);
    assert!(
        snapshot_abs.exists(),
        "expected snapshot at {}",
        snapshot_abs.display()
    );
    let snapshot = std::fs::read_to_string(&snapshot_abs).unwrap();
    assert!(snapshot.contains("blocked dirty done work"));
    assert!(snapshot.contains("blocked.txt"));

    let manager_messages =
        crate::team::inbox::pending_messages(&crate::team::inbox::inboxes_root(&repo), "manager")
            .unwrap();
    assert!(manager_messages.is_empty());
    let engineer_messages =
        crate::team::inbox::pending_messages(&crate::team::inbox::inboxes_root(&repo), "eng-1")
            .unwrap();
    assert!(engineer_messages.is_empty());

    let events = crate::team::events::read_events(&crate::team::team_events_path(&repo)).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "worktree_reconciled"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref().unwrap_or("").contains("eng-1/42")
    }));
    assert!(events.iter().any(|event| {
        event.event == "state_reconciliation"
            && event.role.as_deref() == Some("eng-1")
            && event.reason.as_deref() == Some("done_lane_reset")
    }));
}

#[test]
fn stale_preserve_blocker_on_a_done_task_is_cleared_keeping_status_and_claim() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = make_test_daemon(tmp.path(), vec![]);
    let reason = "Task 17 is blocked because Batty could not safely auto-save eng-1's dirty worktree before merge-conflict recovery. preserve_failed_reset_skipped";
    write_board_task_file(
        tmp.path(),
        17,
        "done-task",
        "done",
        Some("eng-1"),
        &[],
        Some(reason),
    );

    daemon.maybe_clear_stale_preserve_blockers().unwrap();

    let tasks_dir = daemon.board_dir().join("tasks");
    let task = crate::task::load_task_by_id(&tasks_dir, 17).unwrap();
    assert_eq!(task.status, "done");
    assert_eq!(task.claimed_by.as_deref(), Some("eng-1"));
    assert!(task.blocked.is_none() && task.blocked_on.is_none());
}

#[test]
fn preserve_blocker_stays_unless_the_task_is_done_and_its_work_is_on_trunk() {
    let tmp = tempfile::tempdir().unwrap();
    let mut daemon = make_test_daemon(tmp.path(), vec![]);
    let reason = "Batty could not safely auto-save eng-1's dirty worktree before reset. preserve_failed_reset_skipped";
    write_board_task_file(
        tmp.path(),
        21,
        "still-working",
        "in-progress",
        Some("eng-1"),
        &[],
        Some(reason),
    );
    write_board_task_file(
        tmp.path(),
        22,
        "other-blocker",
        "done",
        None,
        &[],
        Some("waiting on owner"),
    );
    write_board_task_file(
        tmp.path(),
        23,
        "unmerged-commit",
        "done",
        None,
        &[],
        Some(reason),
    );
    let tasks_dir = daemon.board_dir().join("tasks");
    let unmerged = std::fs::read_dir(&tasks_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("023")
        })
        .unwrap();
    let text = std::fs::read_to_string(&unmerged).unwrap();
    std::fs::write(
        &unmerged,
        text.replacen("---\nid: 23", "---\ncommit: deadbeefdeadbeef\nid: 23", 1),
    )
    .unwrap();

    daemon.maybe_clear_stale_preserve_blockers().unwrap();

    for id in [21, 22, 23] {
        let task = crate::task::load_task_by_id(&tasks_dir, id).unwrap();
        assert!(
            task.blocked_on.is_some(),
            "task #{id} should keep its blocker"
        );
    }
}
