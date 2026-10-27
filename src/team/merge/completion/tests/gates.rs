use super::*;

#[test]
fn completion_gate_rejects_zero_commits_but_keeps_task_active() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "zero-commit-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

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

    // Task stays active — engineer still owns it (false-done prevention)
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(daemon.retry_count_for_test("eng-1"), None);
}

#[test]
fn zero_commit_completions_escalate_at_max_iterations() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "zero-commit-loop");

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
        .max_iterations = 2;
    for _ in 0..2 {
        daemon.set_active_task_for_test("eng-1", 42);
        daemon.set_member_state_for_test("eng-1", MemberState::Working);
        handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    }

    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join("042-zero-commit-loop.md");
    let task = crate::task::Task::from_file(&task_path).unwrap();
    assert_eq!(task.status, "blocked");
    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.iter().any(|message| {
        message
            .body
            .contains("complete 2 times with no commits ahead of main")
    }));
}

#[test]
fn zero_commit_retry_message_sent() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "zero-commit-message");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

    let manager = MemberInstance {
        name: "manager".to_string(),
        role_name: "manager".to_string(),
        role_type: crate::team::config::RoleType::Manager,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: None,
        use_worktrees: false,
        ..Default::default()
    };
    let engineer = MemberInstance {
        name: "eng-1".to_string(),
        role_name: "eng-1".to_string(),
        role_type: crate::team::config::RoleType::Engineer,
        agent: Some("claude".to_string()),
        prompt: None,
        reports_to: Some("manager".to_string()),
        use_worktrees: true,
        ..Default::default()
    };
    let mut daemon = make_test_daemon(&repo, vec![manager, engineer]);

    daemon.set_active_task_for_test("eng-1", 42);
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert_eq!(engineer_messages[0].from, "batty");
    assert!(
        engineer_messages[0]
            .body
            .contains("no commits ahead of main"),
        "rejection message should mention no commits: {:?}",
        engineer_messages[0].body
    );
    assert!(
        engineer_messages[0].body.contains("git add -A"),
        "rejection message should include commit instructions: {:?}",
        engineer_messages[0].body
    );

    // On first rejection, manager is NOT notified (only after 3 rejections)
    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.is_empty());
}

#[test]
fn scope_violation_completion_reverts_and_emits_event() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_scope_ack_team_config(&repo);
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(repo.join("docs").join("notes.md"), "base notes\n").unwrap();
    git_ok(&repo, &["add", "docs/notes.md"]);
    git_ok(&repo, &["commit", "-m", "add docs notes"]);

    write_task_file_with_body(
        &repo,
        42,
        "scope-violation",
        "Task description.\nSCOPE FENCE: src/lib.rs\n",
    );

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    let ack = inbox::InboxMessage::new_send("eng-1", "manager", "Scope ACK #42");
    inbox::deliver_to_inbox(&inbox::inboxes_root(&repo), &ack).unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { true }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(worktree_dir.join("docs").join("notes.md"), "out of scope\n").unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs", "docs/notes.md"]);
    git_ok(&worktree_dir, &["commit", "-m", "mixed scope change"]);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(
        std::fs::read_to_string(worktree_dir.join("docs").join("notes.md")).unwrap(),
        "base notes\n"
    );
    let revert_subject = String::from_utf8(
        std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(&worktree_dir)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(revert_subject.contains("Restore fenced files after automatic scope recovery"));

    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "scope_fence_violation"
            && event.task.as_deref() == Some("42")
            && event.role.as_deref() == Some("eng-1")
            && event
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("reclaim_requested=true"))
    }));
}

#[test]
fn scope_violation_completion_removes_new_out_of_scope_files_not_on_main() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_scope_ack_team_config(&repo);
    write_task_file_with_body(
        &repo,
        42,
        "scope-violation-new-file",
        "Task description.\nSCOPE FENCE: src/lib.rs\n",
    );

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    let ack = inbox::InboxMessage::new_send("eng-1", "manager", "Scope ACK #42");
    inbox::deliver_to_inbox(&inbox::inboxes_root(&repo), &ack).unwrap();

    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { true }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(worktree_dir.join("stray.txt"), "out of scope\n").unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs", "stray.txt"]);
    git_ok(
        &worktree_dir,
        &["commit", "-m", "mixed scope change with stray file"],
    );

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert!(!worktree_dir.join("stray.txt").exists());
    let revert_subject = String::from_utf8(
        std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(&worktree_dir)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(revert_subject.contains("Restore fenced files after automatic scope recovery"));
}

#[test]
fn narration_only_completion_retries_then_escalates_after_two_rejections() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "narration-only-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    git_ok(
        &worktree_dir,
        &["commit", "--allow-empty", "-m", "commentary only"],
    );

    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), true),
    ];
    let mut daemon = make_test_daemon(&repo, members);
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert!(engineer_messages[0]
        .body
        .contains("Your previous attempt only produced commentary. Execute the actual commands to make the code changes."));
    assert!(
        engineer_messages[0]
            .body
            .contains("git diff --stat main..HEAD")
    );

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(manager_messages.is_empty());
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 2);
    assert!(engineer_messages[1]
        .body
        .contains("Your previous attempt only produced commentary. Execute the actual commands to make the code changes."));

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert_eq!(manager_messages.len(), 1);
    assert!(
        manager_messages[0]
            .body
            .contains("hit the narration-only quality gate 2 times")
    );
    assert!(
        manager_messages[0]
            .body
            .contains("completion diff is narration-only")
    );

    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    let narration_events: Vec<_> = events
        .iter()
        .filter(|event| event.event == "narration_rejection")
        .collect();
    assert_eq!(narration_events.len(), 2);
    assert_eq!(narration_events[0].task.as_deref(), Some("42"));
    assert_eq!(
        narration_events[1].reason.as_deref(),
        Some("rejection_count=2")
    );
}

/// Regression for #635: the narration-rejection counter and the
/// `verification_state.iteration` counter must both remain
/// consistent across a mixed sequence of zero-commit and
/// narration-only attempts. Before the fix it was possible to
/// escalate prematurely because the metadata sources disagreed
/// about how many narration attempts had occurred.
#[test]
fn narration_rejection_counter_survives_mixed_zero_commit_and_narration_attempts() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 77, "mixed-failure-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), true),
    ];
    let mut daemon = make_test_daemon(&repo, members);
    daemon.set_active_task_for_test("eng-1", 77);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    // First attempt: zero commits ahead of main. The completion
    // gate rejects it but does NOT increment the narration-only
    // counter (this is an "insufficient evidence" failure, not
    // narration).
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(
        daemon.verification.narration_rejection_counts.get(&77),
        None,
        "zero-commit rejection must not bump the narration counter"
    );
    assert_eq!(daemon.active_task_id("eng-1"), Some(77));

    // Second attempt: narration-only (empty commit). First
    // narration rejection — counter = 1, no escalation yet.
    git_ok(
        &worktree_dir,
        &["commit", "--allow-empty", "-m", "commentary only"],
    );
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(
        daemon.verification.narration_rejection_counts.get(&77),
        Some(&1),
        "first narration rejection should record count=1"
    );
    assert_eq!(daemon.active_task_id("eng-1"), Some(77));

    // Third attempt: another narration-only. Counter = 2 → escalate.
    git_ok(
        &worktree_dir,
        &["commit", "--allow-empty", "-m", "more commentary"],
    );
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    // After escalation, the counter must be cleared so a future
    // reassignment starts fresh.
    assert!(
        !daemon
            .verification
            .narration_rejection_counts
            .contains_key(&77),
        "escalation should clear the narration counter for the task"
    );

    // And the manager must have been notified — exactly once.
    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert_eq!(
        manager_messages.len(),
        1,
        "manager should see exactly one escalation message across the mixed sequence"
    );
    assert!(
        manager_messages[0]
            .body
            .contains("hit the narration-only quality gate 2 times"),
        "manager escalation must report the accurate narration count, not the iteration count"
    );
}

#[test]
fn narration_rejection_count_is_task_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "first-task");
    write_task_file(&repo, 43, "second-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    git_ok(
        &worktree_dir,
        &["commit", "--allow-empty", "-m", "commentary only"],
    );

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(
        daemon.verification.narration_rejection_counts.get(&42),
        Some(&1)
    );

    daemon.clear_active_task("eng-1");
    assert!(
        !daemon
            .verification
            .narration_rejection_counts
            .contains_key(&42)
    );

    daemon.set_active_task_for_test("eng-1", 43);
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(
        daemon.verification.narration_rejection_counts.get(&43),
        Some(&1)
    );
}

#[test]
fn narration_only_completion_rejects_docs_only_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "docs-only-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::create_dir_all(worktree_dir.join("docs")).unwrap();
    std::fs::write(
        worktree_dir.join("docs").join("notes.md"),
        "commentary only\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "docs/notes.md"]);
    git_ok(&worktree_dir, &["commit", "-m", "docs only"]);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(engineer_messages.len(), 1);
    assert!(engineer_messages[0].body.contains("found no code changes"));
    assert!(engineer_messages[0].body.contains("Code files changed: 0"));

    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    assert!(events.iter().any(|event| {
        event.event == "narration_rejection"
            && event.task.as_deref() == Some("42")
            && event.role.as_deref() == Some("eng-1")
    }));
}

#[test]
fn docs_only_evidence_role_accepts_docs_only_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "docs-role-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::create_dir_all(worktree_dir.join("docs")).unwrap();
    std::fs::write(worktree_dir.join("docs").join("guide.md"), "how to\n").unwrap();
    git_ok(&worktree_dir, &["add", "docs/guide.md"]);
    git_ok(&worktree_dir, &["commit", "-m", "docs: add guide"]);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    let role_name = daemon
        .config
        .members
        .iter()
        .find(|member| member.name == "eng-1")
        .unwrap()
        .role_name
        .clone();
    let roles = &mut daemon.config.team_config.roles;
    roles.retain(|role| role.name != role_name);
    roles.push(crate::team::config::RoleDef {
        name: role_name,
        docs_only_evidence: true,
        ..Default::default()
    });
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert!(
        !engineer_messages
            .iter()
            .any(|message| message.body.contains("found no code changes")),
        "docs-only work from a docs role must not be rejected as narration: {engineer_messages:?}"
    );
    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    assert!(
        !events
            .iter()
            .any(|event| event.event == "narration_rejection")
    );
}

#[test]
fn no_commits_rejection_keeps_assignment() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "no-commits-keep");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

    let mut daemon = setup_completion_daemon(&repo, "eng-1");

    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    // Assignment kept — engineer still owns the task (false-done prevention)
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
}

#[test]
fn no_commits_rejection_does_not_retry_and_keeps_task() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "no-commits-no-retry");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::remove_file(worktree_dir.join("Cargo.toml")).unwrap();

    let mut daemon = setup_completion_daemon(&repo, "eng-1");

    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    // Retry count should not be incremented
    assert_eq!(daemon.retry_count_for_test("eng-1"), None);
    // Active task kept — engineer still owns it (false-done prevention)
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
}
