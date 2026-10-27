use super::*;

#[test]
fn multi_repo_completion_diff_finds_changed_files_inside_subrepos() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "multi-completion");
    let team_config_dir = tmp.path().join(".batty").join("team_config");
    std::fs::create_dir_all(&team_config_dir).unwrap();

    let engineer = "eng-1";
    let base_branch = engineer_base_branch_name(engineer);
    let worktree_root = tmp.path().join(".batty").join("worktrees").join(engineer);
    let sub_worktree = worktree_root.join("repo");
    setup_engineer_worktree(&repo, &sub_worktree, &base_branch, &team_config_dir).unwrap();

    git_ok(&sub_worktree, &["checkout", "-b", "eng-1/42"]);
    std::fs::write(
        sub_worktree.join("src").join("lib.rs"),
        "pub fn smoke() -> bool {\n    false\n}\n",
    )
    .unwrap();
    git_ok(&sub_worktree, &["add", "src/lib.rs"]);
    git_ok(&sub_worktree, &["commit", "-m", "change subrepo file"]);

    let changed =
        multi_repo_files_changed_from_trunk(&worktree_root, &["repo".to_string()], "main").unwrap();
    let code_changed =
        multi_repo_code_files_changed_from_trunk(&worktree_root, &["repo".to_string()], "main")
            .unwrap();
    let diff_stat =
        multi_repo_diff_stat_from_trunk(&worktree_root, &["repo".to_string()], "main").unwrap();

    assert!(
        changed > 0,
        "expected changed files to be counted from the sub-repo"
    );
    assert!(
        code_changed > 0,
        "expected code-file counting to include changed files from the sub-repo"
    );
    assert!(
        diff_stat.contains("[repo]"),
        "diff stat should be labeled by sub-repo, got: {diff_stat}"
    );
    assert_eq!(
        git_stdout(&sub_worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "eng-1/42"
    );
}

#[test]
fn multi_repo_merge_is_all_or_nothing_on_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    let a = init_sub_repo(project, "A");
    let b = init_sub_repo(project, "B");
    add_sub_worktree(project, "A", Some("from engineer A\n"));
    add_sub_worktree(project, "B", Some("from engineer B\n"));
    // Trunk in B moves on and conflicts with the engineer's edit.
    std::fs::write(b.join("shared.txt"), "trunk B\n").unwrap();
    git_ok(&b, &["commit", "-q", "-am", "trunk change"]);
    let a_before = head_of(&a, "main");
    let b_before = head_of(&b, "main");

    let names = vec!["A".to_string(), "B".to_string()];
    let outcome = merge_multi_repo_engineer_branch(project, "eng-1", &names, "main").unwrap();

    assert!(
        matches!(outcome, MergeOutcome::RebaseConflict(_)),
        "expected preflight conflict, got {outcome:?}"
    );
    assert_eq!(head_of(&a, "main"), a_before, "A must not be merged alone");
    assert_eq!(head_of(&b, "main"), b_before);
}

#[test]
fn multi_repo_merge_refuses_to_switch_user_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    let a = init_sub_repo(project, "A");
    add_sub_worktree(project, "A", Some("engineer\n"));
    git_ok(&a, &["checkout", "-q", "-b", "user-feature"]);
    let main_before = head_of(&a, "main");

    let names = vec!["A".to_string()];
    let outcome = merge_multi_repo_engineer_branch(project, "eng-1", &names, "main").unwrap();

    assert!(
        matches!(outcome, MergeOutcome::MergeFailure(_)),
        "{outcome:?}"
    );
    assert_eq!(current_worktree_branch(&a).unwrap(), "user-feature");
    assert_eq!(head_of(&a, "main"), main_before);
}

#[test]
fn multi_repo_merge_does_not_reset_skipped_repos() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    let a = init_sub_repo(project, "A");
    init_sub_repo(project, "C");
    add_sub_worktree(project, "A", Some("engineer\n"));
    let c_wt = add_sub_worktree(project, "C", None);

    let names = vec!["A".to_string(), "C".to_string()];
    let outcome = merge_multi_repo_engineer_branch(project, "eng-1", &names, "main").unwrap();

    assert!(matches!(outcome, MergeOutcome::Success(_)), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(a.join("shared.txt")).unwrap(),
        "engineer\n"
    );
    assert_eq!(
        current_worktree_branch(&c_wt).unwrap(),
        "eng-1/5",
        "repo with nothing merged must keep its lane untouched"
    );
}

#[test]
fn multi_repo_merge_propagates_commit_count_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    init_sub_repo(project, "A");
    add_sub_worktree(project, "A", Some("engineer\n"));
    let names = vec!["A".to_string()];
    assert!(merge_multi_repo_engineer_branch(project, "eng-1", &names, "no-such-trunk").is_err());
}

#[test]
fn completion_diff_analysis_runs_in_sub_repos_for_multi_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path();
    init_sub_repo(project, "A");
    init_sub_repo(project, "B");
    add_sub_worktree(project, "A", Some("engineer A\nmore\n"));
    add_sub_worktree(project, "B", Some("engineer B\n"));
    let worktree_dir = project.join(".batty").join("worktrees").join("eng-1");
    let names = vec!["A".to_string(), "B".to_string()];

    let summary =
        analyze_completion_diff(project, true, &names, &worktree_dir, "main", "eng-1/5").unwrap();
    assert_eq!(summary.files_changed, 2);
    assert!(!summary.has_conflicts);

    // The project root itself is not a git repo: single-repo analysis there
    // must fail (and therefore never auto-merge).
    assert!(
        analyze_completion_diff(project, false, &names, &worktree_dir, "main", "eng-1/5").is_err()
    );
}
