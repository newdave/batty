use super::*;

#[test]
fn completion_uses_verification_test_command_override() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "verification-test-command");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(
        worktree_dir.join("check.sh"),
        "#!/bin/sh\necho verification override\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(worktree_dir.join("check.sh"))
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(worktree_dir.join("check.sh"), perms).unwrap();
    }

    std::fs::write(worktree_dir.join("note.txt"), "done\n").unwrap();
    git_ok(&worktree_dir, &["add", "check.sh", "note.txt"]);
    git_ok(&worktree_dir, &["commit", "-m", "add override command"]);
    seed_completion_packet_metadata(&repo, 42, "verification-test-command", &worktree_dir);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.workflow_policy.test_command = Some("false".to_string());
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .test_command = Some("./check.sh".to_string());
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        std::fs::read_to_string(repo.join("note.txt")).unwrap(),
        "done\n"
    );
    let snapshot =
        std::fs::read_to_string(verification_snapshot_path(&repo, 42, "eng-1", 1)).unwrap();
    assert!(snapshot.contains("\"phase\": \"complete\""));
    assert!(snapshot.contains("\"passed\": true"));
    assert!(!snapshot.contains("test command failed without a parsed failure line"));
}

#[test]
fn completion_skips_automatic_verification_when_disabled() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "verification-skipped");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "introduce failing tests"]);
    seed_completion_packet_metadata(&repo, 42, "verification-skipped", &worktree_dir);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .auto_run_tests = false;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );

    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "verification_evidence_collected"
            && event.reason.as_deref() == Some("automatic verification skipped by policy")
    }));
}

#[test]
fn failing_verification_transitions_to_fixing_with_context() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "verification-fixing");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "introduce failing tests"]);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Working)
    );

    let verification_state = daemon.verification.engineers.get("eng-1").unwrap();
    assert_eq!(verification_state.phase, VerificationPhase::Fixing);
    assert_eq!(verification_state.iteration, 1);
    assert!(!verification_state.last_test_passed);
    assert!(
        verification_state
            .last_test_output
            .as_deref()
            .is_some_and(|output| output.contains("smoke_test"))
    );

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert!(engineer_messages[0].body.contains("Fix attempt 1/2"));
    assert!(
        engineer_messages[0]
            .body
            .contains("Verification failed because the test command did not pass.")
    );
    assert!(
        engineer_messages[0]
            .body
            .contains("Latest verification output:")
    );

    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "verification_phase_changed"
            && event.step.as_deref() == Some("verifying")
            && event.reason.as_deref() == Some("from=executing to=verifying iteration=0")
    }));
    assert!(events.iter().any(|event| {
        event.event == "verification_phase_changed"
            && event.step.as_deref() == Some("fixing")
            && event.reason.as_deref() == Some("from=verifying to=fixing iteration=1")
    }));
    assert!(events.iter().any(|event| {
        event.event == "verification_evidence_collected"
            && event.step.as_deref() == Some("tests_failed")
    }));

    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join("042-verification-fixing.md");
    let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
    assert_eq!(
        metadata.outcome.as_deref(),
        Some("verification_retry_required")
    );
    assert_eq!(metadata.tests_passed, Some(false));
    let snapshot_path = verification_snapshot_path(&repo, 42, "eng-1", 1);
    let snapshot = std::fs::read_to_string(snapshot_path).unwrap();
    assert!(snapshot.contains("\"phase\": \"fixing\""));
    assert!(snapshot.contains("smoke_test"));
}

#[test]
fn task_reference_mismatch_records_branch_recovery_without_consuming_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-branch-recovery-test");
    let engineer = "eng-1-2";
    let task_id = 682;
    let title = "branch-recovery";
    let tasks_dir = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    let task_path = tasks_dir.join(format!("{task_id:03}-{title}.md"));
    std::fs::write(
        &task_path,
        format!(
            "---\nid: {task_id}\ntitle: {title}\nstatus: in-progress\npriority: high\nclaimed_by: {engineer}\nbranch: eng-1-3/{task_id}\nworktree_path: .batty/worktrees/{engineer}\nclass: standard\n---\n\nTask description.\n"
        ),
    )
    .unwrap();

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
    setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &engineer_base_branch_name(engineer),
        &team_config_dir,
    )
    .unwrap();

    checkout_worktree_branch_from_trunk(&worktree_dir, "eng-1-2/682", "main").unwrap();
    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn valid_682() {}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(
        &worktree_dir,
        &["commit", "-m", "Task #682: valid recovery state"],
    );
    let valid_commit = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
    git_ok(
        &worktree_dir,
        &[
            "branch",
            "preserved/eng-1-2-682-branch-lock-recovery-20260424182106",
        ],
    );

    checkout_worktree_branch_from_trunk(&worktree_dir, "eng-1-3/736", "main").unwrap();
    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn wrong_736() {}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "Task #736: wrong branch"]);

    let mut daemon = setup_completion_daemon(&repo, engineer);
    daemon.set_active_task_for_test(engineer, task_id);
    daemon.set_member_state_for_test(engineer, MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, engineer).unwrap();

    assert_eq!(daemon.active_task_id(engineer), Some(task_id));
    assert!(!daemon.verification.engineers.contains_key(engineer));
    assert_eq!(
        current_worktree_branch(&worktree_dir).unwrap(),
        "eng-1-3/736"
    );

    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "in-progress");
    assert!(task.blocked_on.is_none());
    let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
    assert_eq!(
        metadata.outcome.as_deref(),
        Some("branch_recovery_required")
    );
    assert_eq!(metadata.branch.as_deref(), Some("eng-1-2/682"));
    assert_eq!(metadata.commit.as_deref(), Some(valid_commit.as_str()));
    assert_eq!(
        metadata.worktree_path.as_deref(),
        Some(".batty/worktrees/eng-1-2")
    );
    assert_eq!(metadata.tests_passed, Some(false));

    let snapshot =
        std::fs::read_to_string(verification_snapshot_path(&repo, task_id, engineer, 1)).unwrap();
    assert!(snapshot.contains("\"phase\": \"branch_recovery\""));
    assert!(snapshot.contains("\"outcome\": \"branch_recovery_required\""));
    assert!(snapshot.contains("\"actual_branch\": \"eng-1-3/736\""));
    assert!(snapshot.contains("\"expected_branch\": \"eng-1-2/682\""));
    assert!(snapshot.contains("preserved/eng-1-2-682-branch-lock-recovery-20260424182106"));
    assert!(snapshot.contains("\"attempt\": 0"));

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), engineer).unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert!(
        engineer_messages[0]
            .body
            .contains("branch_recovery_required")
    );
    assert!(
        engineer_messages[0]
            .body
            .contains("preserved/eng-1-2-682-branch-lock-recovery-20260424182106")
    );
    assert!(!engineer_messages[0].body.contains("Fix attempt"));

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert_eq!(manager_messages.len(), 1);
    assert!(
        manager_messages[0]
            .body
            .contains("Branch recovery required before verifying task #682")
    );
}

#[test]
fn automation_failed_tests_verification_retry_required_relaunches_assignment() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-retry-redispatch-test");
    write_task_file(&repo, 42, "failing-test-task");
    let board_dir = repo.join(".batty").join("team_config").join("board");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(
        &repo,
        &worktree_dir,
        &engineer_base_branch_name("eng-1"),
        &team_config_dir,
    )
    .unwrap();
    let worktree_path = worktree_dir
        .strip_prefix(&repo)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    checkout_worktree_branch_from_trunk(&worktree_dir, "eng-1/42", "main").unwrap();
    crate::team::task_cmd::assign_task_owners(&board_dir, 42, Some("eng-1"), None).unwrap();
    crate::team::task_cmd::set_task_assignment_context(
        &board_dir,
        42,
        Some("eng-1/42"),
        Some(&worktree_path),
    )
    .unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn test_dispatch_wip_guard() {\n        assert_eq!(2, 3);\n    }\n}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "keep failing retry"]);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.config.team_config.use_shim = true;
    let (parent_sock, child_sock) = socketpair().unwrap();
    let parent_channel = crate::shim::protocol::Channel::new(parent_sock);
    let mut child_channel = crate::shim::protocol::Channel::new(child_sock);
    let mut handle = AgentHandle::new(
        "eng-1".to_string(),
        parent_channel,
        12345,
        "codex".to_string(),
        "codex".to_string(),
        repo.clone(),
    );
    handle.apply_state_change(ShimState::Idle);
    daemon.shim_handles.insert("eng-1".to_string(), handle);
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let command = child_channel.recv().unwrap().unwrap();
    match command {
        Command::SendMessage { from, body, .. } => {
            assert_eq!(from, "batty");
            assert!(body.contains("Fix attempt 1/2"));
            assert!(body.contains("Verification failed because the test command did not pass."));
        }
        other => panic!("expected verification retry SendMessage, got {other:?}"),
    }
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-1/42");
}

#[test]
fn verification_max_iterations_blocks_task_and_escalates_manager() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "verification-failed");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { false }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "introduce failing tests"]);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .max_iterations = 1;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );
    assert!(!daemon.verification.engineers.contains_key("eng-1"));

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert!(
        engineer_messages
            .iter()
            .any(|message| message.body.contains("Fix attempt 1/1"))
    );

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.iter().any(|message| {
        message
            .body
            .contains("hit verification max iterations on task #42")
    }));
    assert!(
        manager_messages
            .iter()
            .any(|message| message.body.contains("Latest phase: failed"))
    );

    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join("042-verification-failed.md");
    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "blocked");
    assert!(
        task.blocked_on
            .as_deref()
            .is_some_and(|reason| reason.contains("verification escalation after 1 attempts"))
    );

    let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
    assert_eq!(metadata.outcome.as_deref(), Some("verification_escalated"));
    let snapshot_path = verification_snapshot_path(&repo, 42, "eng-1", 1);
    let snapshot = std::fs::read_to_string(snapshot_path).unwrap();
    assert!(snapshot.contains("\"phase\": \"failed\""));
    assert!(snapshot.contains("\"passed\": false"));

    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "verification_phase_changed"
            && event.step.as_deref() == Some("failed")
            && event.reason.as_deref() == Some("from=verifying to=failed iteration=1")
    }));
    assert!(events.iter().any(|event| {
        event.event == "verification_max_iterations_reached"
            && event.task.as_deref() == Some("42")
            && event.role.as_deref() == Some("eng-1")
            && event.recipient.as_deref() == Some("manager")
    }));
    assert!(events.iter().any(|event| {
        event.event == "task_escalated"
            && event.task.as_deref() == Some("42")
            && event.reason.as_deref() == Some("verification_failed")
    }));
}

#[test]
fn verification_escalation_on_backlog_task_clears_state_and_does_not_repeat() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "backlog-loop");
    set_task_status(&repo, "042-backlog-loop.md", "in-progress", "backlog");
    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .max_iterations = 1;
    let escalations = |dir: &Path| {
        read_events(&dir.join("events.jsonl"))
            .unwrap()
            .iter()
            .filter(|e| e.event == "verification_max_iterations_reached")
            .count()
    };
    let mut first_escalation_at = None;
    for attempt in 0..4 {
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
        if first_escalation_at.is_none() && escalations(&team_config_dir) > 0 {
            first_escalation_at = Some(attempt);
            break;
        }
    }

    assert!(
        first_escalation_at.is_some(),
        "verification never escalated"
    );
    assert_eq!(escalations(&team_config_dir), 1);
    assert!(!daemon.verification.engineers.contains_key("eng-1"));
    // Backlog -> blocked is illegal; escalation must route via todo.
    let task =
        crate::task::Task::from_file(&team_config_dir.join("board/tasks/042-backlog-loop.md"))
            .unwrap();
    assert_eq!(task.status, "blocked");
}

#[test]
fn test_failure_retry_message_includes_structured_summary() {
    let (_tmp, repo, _worktree_dir) = setup_failing_test_repo("eng-1");
    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert!(
        engineer_messages[0]
            .body
            .contains("1 tests failed: tests::test_dispatch_wip_guard")
    );
    assert!(engineer_messages[0].body.contains("src/lib.rs"));

    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join("042-failing-test-task.md");
    let metadata = crate::team::board::read_workflow_metadata(&task_path).unwrap();
    assert_eq!(metadata.tests_run, Some(true));
    assert_eq!(metadata.tests_passed, Some(false));
    assert_eq!(
        metadata.outcome.as_deref(),
        Some("verification_retry_required")
    );
    assert_eq!(
        metadata.test_results.as_ref().map(|results| results.failed),
        Some(1)
    );
    assert!(
        metadata
            .artifacts
            .iter()
            .any(|artifact| artifact.ends_with("task-042-eng-1-attempt-1.json"))
    );
}

#[test]
fn test_failure_escalation_includes_structured_summary() {
    let (_tmp, repo, _worktree_dir) = setup_failing_test_repo("eng-1");
    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .max_iterations = 1;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.iter().any(|message| {
        message.body.contains("Summary: 1 tests failed")
            && message.body.contains("tests::test_dispatch_wip_guard")
    }));
}

#[test]
fn test_failure_escalation_is_suppressed_when_recently_sent() {
    let (_tmp, repo, _worktree_dir) = setup_failing_test_repo("eng-1");
    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .max_iterations = 1;
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    daemon
        .scheduler
        .recent_escalations
        .insert("tests_failed_42_eng-1".to_string(), Instant::now());

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(
        manager_messages
            .iter()
            .all(|message| !message.body.contains("failed tests after 2 retries")),
        "recent test failure escalation should be suppressed"
    );
}

#[test]
fn slow_verification_does_not_block_and_is_applied_on_a_later_poll() {
    // The command waits for a gate file, so returning at all while the gate
    // is closed proves completion handling did not wait for it (no timing
    // threshold to trip under load).
    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("open");
    let (_tmp, repo, mut daemon) = setup_background_verification(&format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done; exit 0",
        gate.display()
    ));

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    assert!(
        daemon
            .verification
            .background_tests
            .verification
            .contains_key("eng-1")
    );

    // A poll while the command is blocked finds the job still running and
    // changes nothing.
    poll_verification_jobs(&mut daemon).unwrap();
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert!(!verification_snapshot_path(&repo, 42, "eng-1", 1).exists());

    std::fs::write(&gate, "").unwrap();
    wait_for_verification_jobs(&mut daemon).unwrap();
    let snapshot =
        std::fs::read_to_string(verification_snapshot_path(&repo, 42, "eng-1", 1)).unwrap();
    assert!(snapshot.contains("\"phase\": \"complete\""));
    assert!(snapshot.contains("\"passed\": true"));
}

#[test]
fn verification_past_its_timeout_is_killed_and_recorded_as_failed() {
    let (_tmp, repo, mut daemon) = setup_background_verification("sleep 30; exit 0");
    daemon
        .config
        .team_config
        .workflow_policy
        .verification
        .timeout_secs = 1;

    let started = std::time::Instant::now();
    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    wait_for_verification_jobs(&mut daemon).unwrap();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(25),
        "the 30s command must have been killed"
    );
    let state = daemon.verification.engineers.get("eng-1").unwrap();
    assert!(!state.last_test_passed);
    assert!(
        state
            .last_test_output
            .as_deref()
            .is_some_and(|output| output.contains("timed out after 1s")),
        "{:?}",
        state.last_test_output
    );
    let snapshot =
        std::fs::read_to_string(verification_snapshot_path(&repo, 42, "eng-1", 1)).unwrap();
    assert!(snapshot.contains("timed out after 1s"));
}

#[test]
fn repeated_completion_while_verifying_does_not_start_a_second_job() {
    let counter_dir = tempfile::tempdir().unwrap();
    let counter = counter_dir.path().join("runs");
    let (_tmp, _repo, mut daemon) = setup_background_verification(&format!(
        "echo run >> '{}'; sleep 1; exit 0",
        counter.display()
    ));

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    assert_eq!(daemon.verification.background_tests.verification.len(), 1);

    wait_for_verification_jobs(&mut daemon).unwrap();
    let runs = std::fs::read_to_string(&counter).unwrap();
    assert_eq!(runs.lines().count(), 1, "verification ran {runs:?}");
}

#[test]
fn stale_verification_result_after_reassignment_is_discarded() {
    let (_tmp, repo, mut daemon) = setup_background_verification("sleep 1; exit 0");

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    // The engineer moves on to another task while the tests still run.
    daemon.set_active_task_for_test("eng-1", 99);

    wait_for_verification_jobs(&mut daemon).unwrap();
    assert_eq!(daemon.active_task_id("eng-1"), Some(99));
    assert!(!verification_snapshot_path(&repo, 42, "eng-1", 1).exists());
    let task = crate::task::Task::from_file(
        &crate::team::task_cmd::find_task_path(&daemon.board_dir(), 42).unwrap(),
    )
    .unwrap();
    assert_ne!(task.status, "review");
    assert_ne!(task.status, "done");
}

#[test]
fn clearing_the_active_task_cancels_a_running_verification() {
    let (_tmp, _repo, mut daemon) = setup_background_verification("sleep 30; exit 0");

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    assert!(
        daemon
            .verification
            .background_tests
            .verification
            .contains_key("eng-1")
    );
    daemon.clear_active_task("eng-1");
    assert!(daemon.verification.background_tests.verification.is_empty());
}

#[test]
fn stopping_background_tests_kills_the_command_and_re_runs_it_after_restart() {
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("finished");
    let (_tmp, repo, mut daemon) =
        setup_background_verification(&format!("sleep 1; touch '{}'; exit 0", marker.display()));

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    // Give the worker time to spawn the command before killing it.
    std::thread::sleep(Duration::from_millis(300));
    daemon.stop_background_tests();
    assert!(daemon.verification.background_tests.verification.is_empty());
    assert_eq!(
        daemon.verification.background_tests.retriggers.get("eng-1"),
        Some(&42),
        "interrupted verification must be remembered"
    );
    assert_eq!(
        daemon
            .verification
            .background_tests
            .pending_verifications()
            .get("eng-1"),
        Some(&42),
        "interrupted verification must be persisted"
    );
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !marker.exists(),
        "test command should have been killed with its process group"
    );

    // The next poll re-runs the interrupted verification.
    daemon.poll_background_tests().unwrap();
    assert!(daemon.verification.background_tests.retriggers.is_empty());
    assert!(
        daemon
            .verification
            .background_tests
            .verification
            .contains_key("eng-1")
    );
    wait_for_verification_jobs(&mut daemon).unwrap();
    assert!(marker.exists());
    assert!(verification_snapshot_path(&repo, 42, "eng-1", 1).exists());
}
