use super::*;

#[test]
fn completion_gate_passes_with_commits() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "commit-gate-success");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    git_ok(&worktree_dir, &["add", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "add note"]);
    seed_completion_packet_metadata(&repo, 42, "commit-gate-success", &worktree_dir);

    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng-1".to_string(),
        role_type: crate::team::config::RoleType::Engineer,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(&repo, vec![engineer]);

    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("note.txt")).unwrap(),
        "done\n"
    );

    let timing_log = repo.join(".batty").join("test_timing.jsonl");
    let timings = read_test_timing_log(&timing_log).unwrap();
    assert_eq!(timings.len(), 1);
    assert_eq!(timings[0].task_id, 42);
    assert_eq!(timings[0].engineer, "eng-1");
    assert_eq!(timings[0].branch, "eng-1");
    assert!(!timings[0].regression_detected);

    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join("042-commit-gate-success.md");
    let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
    assert_eq!(metadata.tests_run, Some(true));
    assert_eq!(metadata.tests_passed, Some(true));
    assert_eq!(metadata.outcome.as_deref(), Some("verification_passed"));
    let snapshot_path = verification_snapshot_path(&repo, 42, "eng-1", 1);
    assert!(
        metadata
            .artifacts
            .iter()
            .any(|artifact| artifact.ends_with("task-042-eng-1-attempt-1.json"))
    );
    let snapshot = std::fs::read_to_string(snapshot_path).unwrap();
    assert!(snapshot.contains("\"phase\": \"complete\""));
    assert!(snapshot.contains("\"passed\": true"));
}

#[test]
fn rebase_conflict_first_retry_messages_engineer() {
    let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
    write_task_file(&repo, 42, "rebase-conflict-retry");

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert_eq!(engineer_messages[0].from, "batty");
    assert!(
        engineer_messages[0]
            .body
            .contains("Merge conflict during rebase onto main")
    );
}

#[test]
fn rebase_conflict_first_retry_keeps_task_active_and_counts_retry() {
    let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
    write_task_file(&repo, 42, "rebase-conflict-state");

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(daemon.retry_count_for_test("eng-1"), Some(1));
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Working)
    );
}

#[test]
fn rebase_conflict_third_attempt_escalates_to_manager() {
    let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
    write_task_file(&repo, 42, "rebase-conflict-escalation");

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    daemon.increment_retry("eng-1");
    daemon.increment_retry("eng-1");

    complete_task_and_process_queue(&mut daemon, "eng-1");

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.iter().any(|msg| {
        msg.from == "eng-1"
            && msg
                .body
                .contains("unresolvable merge conflicts after 2 retries")
    }));
}

#[test]
fn rebase_conflict_third_attempt_clears_task_and_sets_idle() {
    let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
    write_task_file(&repo, 42, "rebase-conflict-reset");

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    daemon.increment_retry("eng-1");
    daemon.increment_retry("eng-1");

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(daemon.retry_count_for_test("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );
}

#[test]
fn rebase_conflict_third_attempt_records_escalation_event() {
    let (_tmp, repo, _worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-1");
    write_task_file(&repo, 42, "rebase-conflict-event");

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.workflow_policy.auto_merge.enabled = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.increment_retry("eng-1");
    daemon.increment_retry("eng-1");

    complete_task_and_process_queue(&mut daemon, "eng-1");

    let events = crate::team::events::read_events(
        &repo.join(".batty").join("team_config").join("events.jsonl"),
    )
    .unwrap();
    assert!(events.iter().any(|event| {
        event.event == "task_escalated"
            && event.role.as_deref() == Some("eng-1")
            && event.task.as_deref() == Some("42")
    }));
}

#[test]
fn handle_engineer_completion_escalates_isolated_merge_prep_failures_without_crashing() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "merge-blocked-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(worktree_dir.join("feature.rs"), "fn engineer() {}\n").unwrap();
    git_ok(&worktree_dir, &["add", "feature.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);
    seed_completion_packet_metadata(&repo, 42, "merge-blocked-task", &worktree_dir);

    git_ok(&repo, &["checkout", "-b", "root-side-branch"]);
    std::fs::create_dir_all(repo.join(".batty")).unwrap();
    std::fs::write(
        repo.join(".batty").join("integration-worktrees"),
        "not a directory\n",
    )
    .unwrap();

    let members = vec![
        MemberInstance {
            name: "manager".to_string(),
            role_name: "manager".to_string(),
            role_type: crate::team::config::RoleType::Manager,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: false,
            ..Default::default()
        },
        MemberInstance {
            name: "eng-1".to_string(),
            role_name: "eng-1".to_string(),
            role_type: crate::team::config::RoleType::Engineer,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: Some("manager".to_string()),
            use_worktrees: true,
            ..Default::default()
        },
    ];

    let mut daemon = make_test_daemon(&repo, members);
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert_eq!(manager_messages.len(), 1);
    assert_eq!(manager_messages[0].from, "daemon");
    assert!(
        manager_messages[0]
            .body
            .contains("could not be merged to main")
    );
    assert!(
        manager_messages[0]
            .body
            .contains("isolated merge path failed")
    );

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert_eq!(engineer_messages[0].from, "daemon");
    assert!(
        engineer_messages[0]
            .body
            .contains("could not merge it into main")
    );
}

#[test]
fn handle_engineer_completion_emits_performance_regression_event() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "runtime-regression-task");

    let timing_log = repo.join(".batty").join("test_timing.jsonl");
    for task_id in 1..=5 {
        crate::team::artifact::record_test_timing(
            &timing_log,
            &crate::team::artifact::TestTimingRecord {
                task_id,
                engineer: "eng-1".to_string(),
                branch: format!("eng-1/task-{task_id}"),
                measured_at: 1_777_000_000 + task_id as u64,
                duration_ms: 1,
                rolling_average_ms: Some(1),
                regression_pct: Some(0),
                regression_detected: false,
            },
        )
        .unwrap();
    }

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    git_ok(&worktree_dir, &["add", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "add note"]);
    seed_completion_packet_metadata(&repo, 42, "runtime-regression-task", &worktree_dir);

    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng-1".to_string(),
        role_type: crate::team::config::RoleType::Engineer,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(&repo, vec![engineer]);
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    let events = crate::team::events::read_events(
        &repo.join(".batty").join("team_config").join("events.jsonl"),
    )
    .unwrap();
    assert!(events.iter().any(|event| {
        event.event == "performance_regression"
            && event.task.as_deref() == Some("42")
            && event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("runtime_ms="))
    }));

    let timings = read_test_timing_log(&timing_log).unwrap();
    assert_eq!(timings.len(), 6);
    assert!(timings.last().unwrap().regression_detected);
}

#[test]
fn completion_respects_disabled_policy() {
    let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

    let policy = AutoMergePolicy {
        enabled: false,
        ..AutoMergePolicy::default()
    };
    assert!(!policy.enabled);

    let mut daemon = auto_merge_daemon(&repo, policy);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    // With auto-merge disabled the completion merges without a review gate,
    // through the merge queue.
    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );
    // note.txt merged into main
    assert_eq!(
        std::fs::read_to_string(repo.join("note.txt")).unwrap(),
        "done\n"
    );

    // No auto-merge event should be emitted
    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    assert!(
        !events.iter().any(|e| e.event == "task_auto_merged"),
        "no auto-merge event should be emitted when policy is disabled"
    );
}

fn task_board_status(repo: &Path, task_id: u32) -> String {
    let board_dir = repo.join(".batty").join("team_config").join("board");
    crate::task::Task::from_file(
        &crate::team::task_cmd::find_task_path(&board_dir, task_id).unwrap(),
    )
    .unwrap()
    .status
}

fn queued_request(daemon: &crate::team::daemon::TeamDaemon, index: usize) -> MergeRequest {
    daemon.merge.queue.snapshot().queue[index].clone()
}

#[test]
fn direct_completion_is_queued_instead_of_merged_inline() {
    let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");
    let main_before = current_head(&repo);
    let policy = AutoMergePolicy {
        enabled: false,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    // Nothing landed yet: the merge waits in the queue, the task in review.
    assert_eq!(current_head(&repo), main_before);
    assert!(!repo.join("note.txt").exists());
    assert_eq!(task_board_status(&repo, 42), "review");
    assert_eq!(daemon.merge.queue.queued_len(), 1);
    let request = queued_request(&daemon, 0);
    assert_eq!(request.task_id, 42);
    assert_eq!(request.origin, MergeOrigin::DirectCompletion);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));

    daemon.process_merge_queue_for_test().unwrap();
    assert_eq!(
        std::fs::read_to_string(repo.join("note.txt")).unwrap(),
        "done\n"
    );
    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.event == "task_manual_merged" && e.task.as_deref() == Some("42")),
        "a direct completion merge is recorded as a manual merge"
    );
}

#[test]
fn direct_completion_waits_behind_in_flight_queue_merge() {
    let (_tmp, repo, worktree_dir) = setup_auto_merge_repo("eng-1");
    let main_before = current_head(&repo);
    let policy = AutoMergePolicy {
        enabled: false,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);
    daemon.merge.queue.mark_in_flight_for_test(MergeRequest {
        task_id: 7,
        engineer: "eng-2".to_string(),
        branch: "eng-2/7".to_string(),
        worktree_dir: worktree_dir.clone(),
        queued_at: Instant::now(),
        test_passed: true,
        should_post_merge_verify: true,
        test_duration_ms: 0,
        confidence: 1.0,
        files_changed: 1,
        lines_changed: 1,
        origin: MergeOrigin::AutoMerge,
    });

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(
        current_head(&repo),
        main_before,
        "nothing may land on main while a queue merge is being verified"
    );
    assert!(daemon.merge.queue.is_verifying());
    assert_eq!(daemon.merge.queue.queued_len(), 1);
    assert_eq!(queued_request(&daemon, 0).task_id, 42);
}

/// Multi-repo project (root is not a git repo) with sub-repo `A`, an
/// engineer worktree for it carrying a code commit, and task #42 active for
/// eng-1 with auto-merge disabled and verification skipped.
fn setup_multi_repo_direct_merge() -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    crate::team::daemon::TeamDaemon,
) {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().to_path_buf();
    let sub_repo = init_sub_repo(&project, "A");
    let sub_wt = add_sub_worktree(&project, "A", None);
    std::fs::write(sub_wt.join("lib.rs"), "pub fn work() {}\n").unwrap();
    git_ok(&sub_wt, &["add", "lib.rs"]);
    git_ok(&sub_wt, &["commit", "-q", "-m", "engineer work"]);
    // Claimed by eng-1, as in normal operation: the completion scope fence
    // inspects claimed tasks.
    write_task_file(&project, 42, "multi-repo-direct");

    let mut daemon = setup_completion_daemon(&project, "eng-1");
    daemon.is_multi_repo = true;
    daemon.sub_repo_names = vec!["A".to_string()];
    let workflow = &mut daemon.config.team_config.workflow_policy;
    workflow.auto_merge.enabled = false;
    workflow.verification.auto_run_tests = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    (tmp, project, sub_repo, daemon)
}

#[test]
fn multi_repo_direct_merge_defers_while_lock_is_busy_and_retries() {
    let (_tmp, project, sub_repo, mut daemon) = setup_multi_repo_direct_merge();
    let main_before = head_of(&sub_repo, "main");
    let held = crate::team::merge::MergeLock::try_acquire(&project)
        .unwrap()
        .unwrap();

    // Returning at all while the lock is held proves completion did not
    // wait for it.
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(daemon.merge.deferred_direct.len(), 1);
    assert_eq!(head_of(&sub_repo, "main"), main_before);
    assert_eq!(task_board_status(&project, 42), "review");
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));

    // Still busy: the retry defers again without blocking.
    poll_deferred_direct_merges(&mut daemon).unwrap();
    assert_eq!(daemon.merge.deferred_direct.len(), 1);
    assert_eq!(head_of(&sub_repo, "main"), main_before);

    drop(held);
    poll_deferred_direct_merges(&mut daemon).unwrap();
    assert!(daemon.merge.deferred_direct.is_empty());
    assert_ne!(head_of(&sub_repo, "main"), main_before);
    assert!(sub_repo.join("lib.rs").exists());
    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );
}

#[test]
fn multi_repo_direct_merge_refused_while_queue_merge_is_verifying() {
    let (_tmp, project, sub_repo, mut daemon) = setup_multi_repo_direct_merge();
    let main_before = head_of(&sub_repo, "main");
    daemon.merge.queue.mark_in_flight_for_test(MergeRequest {
        task_id: 7,
        engineer: "eng-2".to_string(),
        branch: "eng-2/7".to_string(),
        worktree_dir: project.join(".batty").join("worktrees").join("eng-2"),
        queued_at: Instant::now(),
        test_passed: true,
        should_post_merge_verify: true,
        test_duration_ms: 0,
        confidence: 1.0,
        files_changed: 1,
        lines_changed: 1,
        origin: MergeOrigin::AutoMerge,
    });

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    poll_deferred_direct_merges(&mut daemon).unwrap();

    assert_eq!(daemon.merge.deferred_direct.len(), 1);
    assert_eq!(head_of(&sub_repo, "main"), main_before);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
}

#[test]
fn deferred_direct_merge_is_dropped_when_task_is_reassigned() {
    let (_tmp, project, sub_repo, mut daemon) = setup_multi_repo_direct_merge();
    let main_before = head_of(&sub_repo, "main");
    let held = crate::team::merge::MergeLock::try_acquire(&project)
        .unwrap()
        .unwrap();
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(daemon.merge.deferred_direct.len(), 1);
    drop(held);

    daemon.set_active_task_for_test("eng-1", 43);
    poll_deferred_direct_merges(&mut daemon).unwrap();

    assert!(daemon.merge.deferred_direct.is_empty());
    assert_eq!(head_of(&sub_repo, "main"), main_before);
}

/// Engineer name the scope-ack team config resolves (its manager is
/// `manager`), so the scope ACK reaches the right recipient.
const FENCED_ENGINEER: &str = "eng-1-1";

/// Multi-repo project with sub-repos `A` and `B`; the engineer's worktree
/// adds `A/a.rs` and `B/b.rs`. Task #42 is claimed by the engineer with
/// `fence` as its scope fence, and the scope is acknowledged.
fn setup_multi_repo_fenced_completion(
    fence: &str,
) -> (tempfile::TempDir, PathBuf, crate::team::daemon::TeamDaemon) {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().to_path_buf();
    write_scope_ack_team_config(&project);
    let wt_root = project
        .join(".batty")
        .join("worktrees")
        .join(FENCED_ENGINEER);
    std::fs::create_dir_all(&wt_root).unwrap();
    for (repo, file) in [("A", "a.rs"), ("B", "b.rs")] {
        let repo_dir = init_sub_repo(&project, repo);
        let wt = wt_root.join(repo);
        git_ok(
            &repo_dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("{FENCED_ENGINEER}/42"),
                wt.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(wt.join(file), format!("{repo} work\n")).unwrap();
        git_ok(&wt, &["add", file]);
        git_ok(&wt, &["commit", "-q", "-m", "engineer work"]);
    }
    let tasks_dir = project
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join("042-multi-repo-fenced.md"),
        format!(
            "---\nid: 42\ntitle: multi-repo-fenced\nstatus: in-progress\npriority: high\nclaimed_by: {FENCED_ENGINEER}\nclass: standard\n---\n\nTask description.\nSCOPE FENCE: {fence}\n"
        ),
    )
    .unwrap();
    let ack = inbox::InboxMessage::new_send(FENCED_ENGINEER, "manager", "Scope ACK #42");
    inbox::deliver_to_inbox(&inbox::inboxes_root(&project), &ack).unwrap();

    let mut daemon = setup_completion_daemon(&project, FENCED_ENGINEER);
    daemon.is_multi_repo = true;
    daemon.sub_repo_names = vec!["A".to_string(), "B".to_string()];
    let workflow = &mut daemon.config.team_config.workflow_policy;
    workflow.auto_merge.enabled = false;
    workflow.verification.auto_run_tests = false;
    workflow.test_command = Some("true".to_string());
    daemon.set_active_task_for_test(FENCED_ENGINEER, 42);
    daemon.set_member_state_for_test(FENCED_ENGINEER, MemberState::Working);
    (tmp, project, daemon)
}

fn scope_violation_events(project: &Path) -> usize {
    read_events(
        &project
            .join(".batty")
            .join("team_config")
            .join("events.jsonl"),
    )
    .unwrap_or_default()
    .iter()
    .filter(|event| event.event == "scope_fence_violation")
    .count()
}

#[test]
fn multi_repo_claimed_task_with_in_scope_changes_passes_scope_fence() {
    let (_tmp, project, mut daemon) = setup_multi_repo_fenced_completion("A, B");

    handle_engineer_completion_and_wait(&mut daemon, FENCED_ENGINEER).unwrap();

    assert_eq!(scope_violation_events(&project), 0);
    assert_eq!(
        git_stdout(&project.join("A"), &["show", "main:a.rs"]),
        "A work"
    );
    assert_eq!(
        git_stdout(&project.join("B"), &["show", "main:b.rs"]),
        "B work"
    );
    assert_eq!(daemon.active_task_id(FENCED_ENGINEER), None);
}

#[test]
fn multi_repo_out_of_scope_changes_are_reverted_in_their_sub_repo_only() {
    let (_tmp, project, mut daemon) = setup_multi_repo_fenced_completion("A");
    let wt_root = project
        .join(".batty")
        .join("worktrees")
        .join(FENCED_ENGINEER);
    let a_head = head_of(&wt_root.join("A"), "HEAD");

    handle_engineer_completion_and_wait(&mut daemon, FENCED_ENGINEER).unwrap();

    assert_eq!(scope_violation_events(&project), 1);
    // B's out-of-scope file was removed by a revert commit in B...
    assert!(!wt_root.join("B").join("b.rs").exists());
    let b_subject = git_stdout(&wt_root.join("B"), &["log", "-1", "--format=%s"]);
    assert!(
        b_subject.contains("Restore fenced files after automatic scope recovery"),
        "{b_subject}"
    );
    // ...while A, in scope, is untouched.
    assert_eq!(head_of(&wt_root.join("A"), "HEAD"), a_head);
    assert!(wt_root.join("A").join("a.rs").exists());
}

/// Defer the multi-repo direct merge of task #42 behind a held merge lock.
fn deferred_multi_repo_merge() -> (
    tempfile::TempDir,
    PathBuf,
    PathBuf,
    crate::team::daemon::TeamDaemon,
    crate::team::merge::MergeLock,
) {
    let (tmp, project, sub_repo, mut daemon) = setup_multi_repo_direct_merge();
    let held = crate::team::merge::MergeLock::try_acquire(&project)
        .unwrap()
        .unwrap();
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(daemon.merge.deferred_direct.len(), 1);
    assert_eq!(task_board_status(&project, 42), "review");
    (tmp, project, sub_repo, daemon, held)
}

/// A daemon for the same multi-repo project after a restart or hot reload
/// (no active tasks known).
fn restarted_multi_repo_daemon(project: &Path) -> crate::team::daemon::TeamDaemon {
    let mut daemon = setup_completion_daemon(project, "eng-1");
    daemon.is_multi_repo = true;
    daemon.sub_repo_names = vec!["A".to_string()];
    let workflow = &mut daemon.config.team_config.workflow_policy;
    workflow.auto_merge.enabled = false;
    workflow.verification.auto_run_tests = false;
    daemon
}

#[test]
fn deferred_direct_merge_survives_restart_and_lands_when_lock_frees() {
    let (_tmp, project, sub_repo, daemon, held) = deferred_multi_repo_merge();
    let main_before = head_of(&sub_repo, "main");
    drop(daemon);

    let mut restarted = restarted_multi_repo_daemon(&project);
    restarted.restore_merge_queue_state();
    assert_eq!(restarted.merge.deferred_direct.len(), 1);
    assert_eq!(restarted.merge.deferred_direct[0].task_id, 42);

    poll_deferred_direct_merges(&mut restarted).unwrap();
    assert_eq!(restarted.merge.deferred_direct.len(), 1, "still waiting");
    assert_eq!(head_of(&sub_repo, "main"), main_before);

    drop(held);
    poll_deferred_direct_merges(&mut restarted).unwrap();
    assert!(restarted.merge.deferred_direct.is_empty());
    assert_ne!(head_of(&sub_repo, "main"), main_before);
    assert!(sub_repo.join("lib.rs").exists());

    // The landed merge is gone from the persisted state too.
    let mut again = restarted_multi_repo_daemon(&project);
    again.restore_merge_queue_state();
    assert!(again.merge.deferred_direct.is_empty());
}

#[test]
fn restored_deferred_direct_merge_for_reassigned_task_is_dropped() {
    let (_tmp, project, sub_repo, daemon, held) = deferred_multi_repo_merge();
    let main_before = head_of(&sub_repo, "main");
    drop(daemon);
    // Task #42 was handed to another engineer while the daemon was down.
    let task_path = crate::team::task_cmd::find_task_path(
        &project.join(".batty").join("team_config").join("board"),
        42,
    )
    .unwrap();
    let content = std::fs::read_to_string(&task_path).unwrap();
    std::fs::write(
        &task_path,
        content.replace("claimed_by: eng-1", "claimed_by: eng-2"),
    )
    .unwrap();

    let mut restarted = restarted_multi_repo_daemon(&project);
    restarted.restore_merge_queue_state();
    assert!(restarted.merge.deferred_direct.is_empty());

    drop(held);
    poll_deferred_direct_merges(&mut restarted).unwrap();
    assert_eq!(head_of(&sub_repo, "main"), main_before);
}

#[test]
fn long_deferred_direct_merge_escalates_once() {
    let (_tmp, project, _sub_repo, mut daemon, _held) = deferred_multi_repo_merge();
    let escalations = |project: &Path| {
        inbox::pending_messages(&inbox::inboxes_root(project), "manager")
            .unwrap()
            .into_iter()
            .filter(|message| message.body.contains("has waited"))
            .count()
    };
    poll_deferred_direct_merges(&mut daemon).unwrap();
    assert_eq!(escalations(&project), 0);

    daemon.merge.deferred_direct[0].deferred_since -= 31 * 60;
    poll_deferred_direct_merges(&mut daemon).unwrap();
    poll_deferred_direct_merges(&mut daemon).unwrap();
    assert_eq!(escalations(&project), 1);
    assert!(daemon.merge.deferred_direct[0].escalated);
    assert_eq!(daemon.merge.deferred_direct.len(), 1, "still retried");
}
