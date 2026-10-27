use super::*;

#[test]
fn completion_auto_merges_small_clean_diff() {
    let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.queued_merge_count_for_test(), 1);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Working)
    );

    let task = crate::task::Task::from_file(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-auto-merge-task.md"),
    )
    .unwrap();
    assert_eq!(task.status, "review");

    daemon.process_merge_queue_for_test().unwrap();

    assert_eq!(daemon.queued_merge_count_for_test(), 0);
    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );

    // note.txt should be merged into main
    assert_eq!(
        std::fs::read_to_string(repo.join("note.txt")).unwrap(),
        "done\n"
    );

    // Verify auto-merge event was emitted
    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    let auto_merge_events: Vec<_> = events
        .iter()
        .filter(|e| e.event == "task_auto_merged")
        .collect();
    assert_eq!(auto_merge_events.len(), 1);
    assert_eq!(auto_merge_events[0].role.as_deref(), Some("eng-1"));
    assert_eq!(auto_merge_events[0].task.as_deref(), Some("42"));
    let decision_event = events
        .iter()
        .find(|e| e.event == "auto_merge_decision_recorded")
        .expect("should record auto-merge decision");
    assert_eq!(decision_event.action_type.as_deref(), Some("accepted"));
    assert!(
        decision_event
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("accepted for auto-merge")),
        "decision reason should explain acceptance: {:?}",
        decision_event.reason
    );
    let post_verify_event = events
        .iter()
        .find(|e| e.event == "auto_merge_post_verify_result")
        .expect("should record post-merge verification result");
    assert_eq!(post_verify_event.success, Some(true));
    assert_eq!(post_verify_event.reason.as_deref(), Some("passed"));
}

#[test]
fn completion_auto_merges_rust_source_plus_docs_diff() {
    let (_tmp, repo, worktree_dir) = setup_auto_merge_repo_without_work("eng-1", "rust-docs-task");
    std::fs::create_dir_all(worktree_dir.join("docs")).unwrap();
    std::fs::write(
        worktree_dir.join("src").join("feature.rs"),
        "pub fn feature_flag() -> &'static str { \"ready\" }\n",
    )
    .unwrap();
    std::fs::write(
        worktree_dir.join("docs").join("feature.md"),
        "Feature notes.\n",
    )
    .unwrap();
    commit_all(&worktree_dir, "rust source plus docs");

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.queued_merge_count_for_test(), 1);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_auto_merge_decision(&repo, "accepted", "accepted for auto-merge");
}

#[test]
fn completion_auto_merges_config_plus_board_metadata_diff() {
    let (_tmp, repo, worktree_dir) =
        setup_auto_merge_repo_without_work("eng-1", "config-board-task");
    std::fs::create_dir_all(worktree_dir.join("config")).unwrap();
    std::fs::create_dir_all(
        worktree_dir
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("metadata"),
    )
    .unwrap();
    std::fs::write(
        worktree_dir.join("config").join("agent.yml"),
        "mode: automatic\n",
    )
    .unwrap();
    std::fs::write(
        worktree_dir
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("metadata")
            .join("release-lanes.md"),
        "release lane metadata\n",
    )
    .unwrap();
    commit_all(&worktree_dir, "config plus board metadata");

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.queued_merge_count_for_test(), 1);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_auto_merge_decision(&repo, "accepted", "accepted for auto-merge");
}

#[test]
fn completion_auto_merges_deleted_file_plus_modified_source_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-auto-merge-test");
    write_task_file(&repo, 42, "delete-modify-task");
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    std::fs::write(repo.join("docs").join("obsolete.md"), "old notes\n").unwrap();
    std::fs::write(
        repo.join("src").join("cleanup.rs"),
        "pub fn cleaned_docs() -> bool { false }\n",
    )
    .unwrap();
    git_ok(&repo, &["add", "docs/obsolete.md", "src/cleanup.rs"]);
    git_ok(
        &repo,
        &["commit", "-m", "add obsolete docs and cleanup helper"],
    );

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    std::fs::remove_file(worktree_dir.join("docs").join("obsolete.md")).unwrap();
    std::fs::write(
        worktree_dir.join("src").join("cleanup.rs"),
        "pub fn cleaned_docs() -> bool { true }\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "-A"]);
    git_ok(
        &worktree_dir,
        &["commit", "-m", "delete obsolete docs and update source"],
    );

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.queued_merge_count_for_test(), 1);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert_auto_merge_decision(&repo, "accepted", "accepted for auto-merge");
}

#[test]
fn completion_routes_generated_report_artifacts_to_review() {
    let (_tmp, repo, worktree_dir) =
        setup_auto_merge_repo_without_work("eng-1", "report-artifact-task");
    std::fs::write(
        worktree_dir.join("src").join("report.rs"),
        "pub fn report_ready() -> bool { true }\n",
    )
    .unwrap();
    let report_dir = worktree_dir.join(".batty").join("reports").join("release");
    std::fs::create_dir_all(&report_dir).unwrap();
    std::fs::write(report_dir.join("readiness.json"), "{\"generated\":true}\n").unwrap();
    commit_all(&worktree_dir, "source plus generated report artifact");

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.queued_merge_count_for_test(), 0);
    assert!(!repo.join(".batty").join("reports").join("release").exists());
    let task = crate::task::Task::from_file(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-report-artifact-task.md"),
    )
    .unwrap();
    assert_eq!(task.status, "review");
    assert_eq!(task.review_owner.as_deref(), Some("manager"));
    assert_auto_merge_decision(&repo, "manual_review", "generated/report artifacts");

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(
        manager_messages
            .iter()
            .any(|message| message.body.contains("generated/report artifacts")),
        "manager should receive generated/report artifact reason: {:?}",
        manager_messages
    );
}

#[test]
fn handle_engineer_completion_records_packet_metadata_for_auto_merge() {
    // Regression test for the "missing_packet" silent throughput killer:
    // when the daemon decides to auto-merge a passing task, it used to
    // move the task to review and enqueue a merge request without ever
    // writing `branch`, `commit`, `worktree_path`, `tests_run`,
    // `tests_passed`, or `outcome` to the task's workflow metadata. The
    // merge queue would then skip the request every time with
    // `missing_packet; branch marker missing; commit marker missing;
    // worktree marker missing`, preventing the auto-merge from ever
    // landing. Verify the daemon now writes those markers itself.
    let (_tmp, repo, worktree_dir) = setup_auto_merge_repo("eng-1");
    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    let task_path = repo
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks")
        .join("042-auto-merge-task.md");
    let metadata = crate::team::board::read_workflow_metadata(&task_path).expect("read metadata");
    let expected_branch = current_worktree_branch(&worktree_dir).unwrap();
    assert_eq!(
        metadata.branch.as_deref(),
        Some(expected_branch.as_str()),
        "branch marker should match the worktree's HEAD branch"
    );
    assert!(
        metadata
            .commit
            .as_deref()
            .is_some_and(|commit| commit.len() >= 7),
        "commit marker should be populated with the worktree HEAD SHA, got {:?}",
        metadata.commit
    );
    assert!(
        metadata
            .worktree_path
            .as_deref()
            .is_some_and(|path| !path.trim().is_empty()),
        "worktree_path marker should be populated"
    );
    assert_eq!(metadata.tests_run, Some(true));
    assert_eq!(metadata.tests_passed, Some(true));
    // The daemon's verification snapshot writes outcome=verification_passed
    // on the Complete phase; the merge queue treats any non-empty outcome
    // other than `verification_retry_required` / `verification_escalated`
    // as merge-ready, so both `verification_passed` and `ready_for_review`
    // are acceptable here. What matters is that *some* merge-ready outcome
    // is present — the previous bug left the field unset.
    let outcome = metadata.outcome.as_deref().unwrap_or("");
    assert!(
        !outcome.is_empty()
            && outcome != "verification_retry_required"
            && outcome != "verification_escalated",
        "outcome marker should be merge-ready, got {outcome:?}"
    );
}

#[test]
fn record_completion_packet_metadata_rejects_branch_task_mismatch_without_writing() {
    let (_tmp, repo, worktree_dir) = setup_auto_merge_repo("eng-1");
    let board_dir = repo.join(".batty").join("team_config").join("board");
    let task_path = board_dir.join("tasks").join("042-auto-merge-task.md");

    let error = record_completion_packet_metadata(
        &repo,
        &board_dir,
        42,
        "eng-1/task-99",
        &worktree_dir,
        true,
        None,
    )
    .unwrap_err()
    .to_string();

    assert!(error.contains("branch `eng-1/task-99` references task(s) #99"));
    let metadata = crate::team::board::read_workflow_metadata(&task_path).expect("read metadata");
    assert!(metadata.branch.is_none());
    assert!(metadata.commit.is_none());
    assert_eq!(metadata.tests_run, None);
}

#[test]
fn completion_routes_large_diff_to_review() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-auto-merge-test");
    write_task_file(&repo, 42, "large-diff-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    // Create a large diff: many files across multiple modules
    for i in 0..10 {
        let dir = worktree_dir.join(format!("module_{i}"));
        std::fs::create_dir_all(&dir).unwrap();
        let content: String = (0..50).map(|j| format!("line {j}\n")).collect();
        std::fs::write(dir.join("file.rs"), content).unwrap();
    }
    git_ok(&worktree_dir, &["add", "."]);
    git_ok(&worktree_dir, &["commit", "-m", "large change"]);

    let policy = AutoMergePolicy {
        enabled: true,
        max_files_changed: 5,
        max_diff_lines: 200,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    let task = crate::task::Task::from_file(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-large-diff-task.md"),
    )
    .unwrap();
    assert_eq!(task.status, "review");
    assert_eq!(task.review_owner.as_deref(), Some("manager"));
    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert_eq!(
        daemon.member_state_for_test("eng-1"),
        Some(MemberState::Idle)
    );

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(
        manager_messages
            .iter()
            .any(|m| m.body.contains("manual review")),
        "manager should receive manual review message: {:?}",
        manager_messages
    );
    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    let decision_event = events
        .iter()
        .find(|e| e.event == "auto_merge_decision_recorded")
        .expect("should record manual review decision");
    assert_eq!(decision_event.action_type.as_deref(), Some("manual_review"));
    assert!(
        decision_event
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("manual review")),
        "decision reason should explain manual review: {:?}",
        decision_event.reason
    );
}

#[test]
fn move_task_to_review_requires_commits_ahead_of_main() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-review-gate-test");
    write_task_file(&repo, 42, "review-gate-task");

    let team_config_dir = repo.join(".batty").join("team_config");
    let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

    let mut daemon = make_test_daemon(
        &repo,
        vec![
            manager_member("manager", None),
            engineer_member("eng-1", Some("manager"), true),
        ],
    );

    let moved = move_task_to_review(
        &mut daemon,
        &repo.join(".batty").join("team_config").join("board"),
        42,
        Some("manager"),
        "eng-1",
    )
    .unwrap();

    assert!(
        !moved,
        "review transition should be blocked without commits"
    );

    let task = crate::task::Task::from_file(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks")
            .join("042-review-gate-task.md"),
    )
    .unwrap();
    assert_eq!(task.status, "in-progress");
    assert!(task.review_owner.is_none());

    let engineer_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert!(
        engineer_messages
            .iter()
            .any(|m| m.body.contains("no commits ahead of main")),
        "engineer should receive review-blocked message: {:?}",
        engineer_messages
    );

    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(
        manager_messages.is_empty(),
        "manager should not be paged for a blocked false-review: {:?}",
        manager_messages
    );
}

#[test]
fn completion_respects_per_task_override() {
    let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);
    daemon.merge.set_auto_merge_override(42, false); // Force manual review

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    // Manager should have received a message about override
    let manager_messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(
        manager_messages
            .iter()
            .any(|m| m.body.contains("Auto-merge disabled by override")),
        "manager should receive override message: {:?}",
        manager_messages
    );
}

#[test]
fn auto_merge_emits_event() {
    let (_tmp, repo, _worktree_dir) = setup_auto_merge_repo("eng-1");

    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(&repo, policy);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    let events_path = repo.join(".batty").join("team_config").join("events.jsonl");
    let events = read_events(&events_path).unwrap();
    let auto_event = events
        .iter()
        .find(|e| e.event == "task_auto_merged")
        .expect("should have task_auto_merged event");

    assert_eq!(auto_event.role.as_deref(), Some("eng-1"));
    assert_eq!(auto_event.task.as_deref(), Some("42"));
    // Confidence should be stored in load field
    assert!(auto_event.load.is_some());
    let confidence = auto_event.load.unwrap();
    assert!(
        confidence > 0.0 && confidence <= 1.0,
        "confidence should be between 0 and 1, got {}",
        confidence
    );
    // Reason should contain files and lines info
    assert!(
        auto_event
            .reason
            .as_ref()
            .is_some_and(|r| r.contains("files=") && r.contains("lines=")),
        "reason should contain diff stats: {:?}",
        auto_event.reason
    );
}
