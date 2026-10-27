//! Low-level git helpers used by the merge subsystem.
//!
//! These wrappers run `git` as a subprocess and attach human-readable context
//! to errors so that callers (and operators reading logs) can tell which
//! high-level operation failed and why.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

/// Run git and return its output only if it exited successfully.
///
/// A nonzero exit becomes an error carrying the command, directory, intent,
/// and stderr. Use [`run_git_with_context_allow_fail`] when the caller
/// inspects the exit status itself (probes, conflict detection, best-effort
/// cleanup).
pub(crate) fn run_git_with_context(
    repo_dir: &Path,
    args: &[&str],
    intent: &str,
) -> Result<crate::git::GitOutput> {
    let output = run_git_with_context_allow_fail(repo_dir, args, intent)?;
    if !output.status.success() {
        bail!(
            "{} ({})",
            describe_git_failure(repo_dir, args, intent, &output.stderr_str()),
            output.status
        );
    }
    Ok(output)
}

/// Run git and return its output whatever the exit status. Only spawn
/// failures and timeouts are errors; the caller must check `output.status`.
pub(crate) fn run_git_with_context_allow_fail(
    repo_dir: &Path,
    args: &[&str],
    intent: &str,
) -> Result<crate::git::GitOutput> {
    let command = format!("git {}", args.join(" "));
    crate::git::cmd(repo_dir)
        .args(args)
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed while trying to {intent}: could not execute `{command}` in {}",
                repo_dir.display()
            )
        })
}

/// Abort an in-progress merge left behind by a failed `git merge`, so the
/// checkout does not stay in a MERGING state with conflict markers.
pub(crate) fn abort_failed_merge(repo_dir: &Path) {
    let merging = run_git_with_context_allow_fail(
        repo_dir,
        &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
        "check for in-progress merge",
    )
    .is_ok_and(|output| output.status.success());
    if !merging {
        return;
    }
    match run_git_with_context_allow_fail(repo_dir, &["merge", "--abort"], "abort failed merge") {
        Ok(output) if output.status.success() => {
            info!(repo = %repo_dir.display(), "aborted failed merge");
        }
        Ok(output) => warn!(
            repo = %repo_dir.display(),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "failed to abort merge"
        ),
        Err(error) => warn!(repo = %repo_dir.display(), error = %error, "failed to abort merge"),
    }
}

pub(crate) fn describe_git_failure(
    repo_dir: &Path,
    args: &[&str],
    intent: &str,
    stderr: &str,
) -> String {
    format!(
        "failed while trying to {intent}: `git {}` in {} returned: {}",
        args.join(" "),
        repo_dir.display(),
        stderr.trim()
    )
}

#[cfg(test)]
pub(crate) fn commits_ahead_of_main(worktree_dir: &Path) -> Result<u32> {
    commits_ahead_of_trunk(worktree_dir, "main")
}

pub(crate) fn commits_ahead_of_trunk(worktree_dir: &Path, trunk_branch: &str) -> Result<u32> {
    crate::worktree::ops::ahead_count(worktree_dir, trunk_branch, "HEAD").with_context(|| {
        format!(
            "failed while trying to count commits ahead of {trunk_branch} before accepting engineer completion: `git rev-list --count {trunk_branch}..HEAD` in {}",
            worktree_dir.display()
        )
    })
}

/// Return the diff stat between main and HEAD using `git diff --stat main..HEAD`.
/// An empty string means the branch has no material file changes relative to main.
#[cfg(test)]
pub(crate) fn diff_stat_from_main(worktree_dir: &Path) -> Result<String> {
    diff_stat_from_trunk(worktree_dir, "main")
}

pub(crate) fn diff_stat_from_trunk(worktree_dir: &Path, trunk_branch: &str) -> Result<String> {
    let range = format!("{trunk_branch}...HEAD");
    let output = run_git_with_context_allow_fail(
        worktree_dir,
        &["diff", "--stat", &range],
        &format!("check diff stat between {trunk_branch} and HEAD for narration-only detection"),
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            describe_git_failure(
                worktree_dir,
                &["diff", "--stat", &range],
                &format!(
                    "check diff stat between {trunk_branch} and HEAD for narration-only detection"
                ),
                &stderr,
            )
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) fn changed_paths_from_trunk(
    worktree_dir: &Path,
    trunk_branch: &str,
) -> Result<Vec<PathBuf>> {
    let range = format!("{trunk_branch}...HEAD");
    let output = run_git_with_context_allow_fail(
        worktree_dir,
        &["diff", "--name-only", &range],
        &format!("list changed paths between {trunk_branch} and HEAD for narration-only detection"),
    )?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{}",
            describe_git_failure(
                worktree_dir,
                &["diff", "--name-only", &range],
                &format!(
                    "list changed paths between {trunk_branch} and HEAD for narration-only detection"
                ),
                &stderr,
            )
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

fn is_completion_noise_path(path: &Path) -> bool {
    let first_component = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();

    if matches!(
        first_component,
        ".batty" | ".batty-target" | ".agents" | ".claude" | "target"
    ) {
        return true;
    }

    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("AGENTS.md" | "CLAUDE.md")
    )
}

fn evidence_paths_from_trunk(worktree_dir: &Path, trunk_branch: &str) -> Result<Vec<PathBuf>> {
    Ok(changed_paths_from_trunk(worktree_dir, trunk_branch)?
        .into_iter()
        .filter(|path| !is_completion_noise_path(path))
        .collect())
}

pub(crate) fn is_non_code_path(path: &Path) -> bool {
    let root_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if matches!(
        root_name,
        "README" | "README.md" | "CHANGELOG.md" | "LICENSE" | "LICENSE.md"
    ) {
        return true;
    }

    let first_component = path
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .unwrap_or_default();
    if matches!(first_component, "docs" | "planning" | "assets") {
        return true;
    }

    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "md" | "markdown"
                | "rst"
                | "adoc"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "svg"
                | "webp"
                | "bmp"
                | "ico"
                | "pdf"
        )
    )
}

/// Count files changed between main and HEAD.
#[cfg(test)]
pub(crate) fn files_changed_from_main(worktree_dir: &Path) -> Result<u32> {
    files_changed_from_trunk(worktree_dir, "main")
}

pub(crate) fn files_changed_from_trunk(worktree_dir: &Path, trunk_branch: &str) -> Result<u32> {
    Ok(evidence_paths_from_trunk(worktree_dir, trunk_branch)?.len() as u32)
}

/// Count code-relevant files changed between main and HEAD.
#[cfg(test)]
pub(crate) fn code_files_changed_from_main(worktree_dir: &Path) -> Result<u32> {
    code_files_changed_from_trunk(worktree_dir, "main")
}

pub(crate) fn code_files_changed_from_trunk(
    worktree_dir: &Path,
    trunk_branch: &str,
) -> Result<u32> {
    Ok(evidence_paths_from_trunk(worktree_dir, trunk_branch)?
        .into_iter()
        .filter(|path| !is_non_code_path(path))
        .count() as u32)
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Preserve uncommitted changes via auto-commit, then force-clean the worktree
/// so `checkout -B` can succeed.
/// Best-effort: failures are logged but do not block the reset attempt.
pub(crate) fn force_clean_worktree(worktree_dir: &Path, engineer_name: &str) {
    let branch = run_git_with_context_allow_fail(
        worktree_dir,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        "capture current branch before worktree cleanup",
    )
    .ok()
    .and_then(|output| {
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    })
    .unwrap_or_else(|| engineer_name.to_string());
    let commit_message = format!("wip: auto-save before worktree reset [{branch}]");
    match crate::worktree::prepare_worktree_for_reset(
        worktree_dir,
        &commit_message,
        std::time::Duration::from_secs(5),
        crate::worktree::PreserveFailureMode::ForceReset,
        "merge/worktree-cleanup",
    ) {
        Ok(reason) => info!(
            engineer = engineer_name,
            reset_reason = reason.as_str(),
            "prepared merge worktree for cleanup"
        ),
        Err(error) => {
            warn!(
                engineer = engineer_name,
                error = %error,
                "worktree cleanup failed during preserve-before-reset preparation"
            );
        }
    }
}

/// Commit at `HEAD` in `worktree_dir`.
pub(crate) fn head_commit(worktree_dir: &Path) -> Result<String> {
    let output = crate::git::cmd(worktree_dir)
        .args(["rev-parse", "HEAD"])
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed to invoke git rev-parse in {}",
                worktree_dir.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed in {}: {}",
            worktree_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Commit `branch` points at, or `None` when it cannot be resolved.
pub(crate) fn branch_head(project_root: &Path, branch: &str) -> Option<String> {
    let output = crate::git::cmd(project_root)
        .args(["rev-parse", "--verify", branch])
        .allow_fail()
        .run()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!commit.is_empty()).then_some(commit)
}

/// Resolve `rev` in `repo`, failing when it does not exist.
pub(crate) fn rev_parse(repo: &Path, rev: &str) -> Result<String> {
    let output = run_git_with_context(
        repo,
        &["rev-parse", "--verify", rev],
        &format!("resolve {rev}"),
    )?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn path_exists_on_trunk(worktree_dir: &Path, trunk_branch: &str, file: &str) -> Result<bool> {
    let output = crate::git::cmd(worktree_dir)
        .args(["ls-tree", "-r", "--name-only", trunk_branch, "--", file])
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed to inspect whether {file} exists on {trunk_branch} in {}",
                worktree_dir.display()
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "failed to inspect whether {file} exists on {trunk_branch} in {}: {}",
            worktree_dir.display(),
            stderr.trim()
        );
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

/// Restore `out_of_scope` paths to their trunk version (removing paths that
/// do not exist on trunk) and commit only those paths. Returns whether a
/// revert commit was made.
pub(crate) fn revert_out_of_scope_files(
    worktree_dir: &Path,
    out_of_scope: &[String],
    trunk_branch: &str,
) -> Result<bool> {
    if out_of_scope.is_empty() {
        return Ok(false);
    }

    let mut restored = Vec::new();
    for file in out_of_scope {
        if path_exists_on_trunk(worktree_dir, trunk_branch, file)? {
            // `checkout <rev> -- <path>` updates both the index and the worktree.
            run_git_with_context(
                worktree_dir,
                &["checkout", trunk_branch, "--", file],
                "revert out-of-scope file",
            )?;
            restored.push(file.as_str());
        } else {
            run_git_with_context(
                worktree_dir,
                &["rm", "-f", "--ignore-unmatch", "--", file],
                "remove out-of-scope file added outside the fence",
            )?;
        }
    }

    // Stage only the fenced paths; never sweep up unrelated worktree changes.
    if !restored.is_empty() {
        let mut add_args = vec!["add", "-A", "--"];
        add_args.extend(restored.iter().copied());
        run_git_with_context(worktree_dir, &add_args, "stage scope revert")?;
    }

    let mut staged_args = vec!["diff", "--cached", "--name-only", "--"];
    staged_args.extend(out_of_scope.iter().map(String::as_str));
    let staged = run_git_with_context(worktree_dir, &staged_args, "inspect staged scope revert")?;
    let staged_paths: Vec<String> = String::from_utf8_lossy(&staged.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    if staged_paths.is_empty() {
        return Ok(false);
    }

    let mut commit_args = vec![
        "commit",
        "-m",
        "Restore fenced files after automatic scope recovery",
        "-m",
        "Constraint: Scope fence violations must be reverted automatically\nConfidence: medium\nScope-risk: narrow\nDirective: This daemon-owned recovery commit keeps the branch clean after out-of-scope writes\nTested: scope fence auto-revert path\nNot-tested: multi-repo scope recovery",
        "--only",
        "--",
    ];
    commit_args.extend(staged_paths.iter().map(String::as_str));
    run_git_with_context(worktree_dir, &commit_args, "commit scope revert")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{git_ok, git_stdout, init_git_repo};

    #[test]
    fn revert_out_of_scope_files_commits_only_fenced_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "scope-revert");
        git_ok(&repo, &["checkout", "-b", "eng-1/9"]);
        std::fs::write(repo.join("src/lib.rs"), "pub fn out_of_scope() {}\n").unwrap();
        std::fs::write(repo.join("added.txt"), "added\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-m", "work"]);
        // Unrelated staged + unstaged work must not be swept into the revert.
        std::fs::write(repo.join("keep.txt"), "keep\n").unwrap();
        git_ok(&repo, &["add", "keep.txt"]);
        std::fs::write(repo.join("Cargo.toml"), "# edited\n").unwrap();

        let files = vec!["src/lib.rs".to_string(), "added.txt".to_string()];
        assert!(revert_out_of_scope_files(&repo, &files, "main").unwrap());

        let committed = git_stdout(&repo, &["show", "--name-only", "--format=", "HEAD"]);
        let mut committed: Vec<&str> = committed.lines().collect();
        committed.sort();
        assert_eq!(committed, vec!["added.txt", "src/lib.rs"]);
        assert_eq!(
            git_stdout(&repo, &["diff", "--cached", "--name-only"]),
            "keep.txt"
        );
        assert_eq!(git_stdout(&repo, &["diff", "--name-only"]), "Cargo.toml");
        assert_eq!(
            std::fs::read_to_string(repo.join("src/lib.rs")).unwrap(),
            git_stdout(&repo, &["show", "main:src/lib.rs"]) + "\n"
        );

        // Nothing left to revert: report false rather than a phantom revert.
        assert!(!revert_out_of_scope_files(&repo, &files, "main").unwrap());
    }

    #[test]
    fn revert_out_of_scope_files_propagates_git_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "scope-revert-fail");
        let err = revert_out_of_scope_files(&repo, &["src/lib.rs".to_string()], "no-such-trunk")
            .unwrap_err();
        assert!(format!("{err:#}").contains("no-such-trunk"), "{err:#}");
    }

    #[test]
    fn abort_failed_merge_clears_conflicted_merge_state() {
        use crate::team::test_support::{git, git_ok, init_git_repo};
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "abort-merge-test");
        std::fs::write(repo.join("file.txt"), "base\n").unwrap();
        git_ok(&repo, &["add", "file.txt"]);
        git_ok(&repo, &["commit", "-m", "base"]);
        git_ok(&repo, &["checkout", "-b", "feature"]);
        std::fs::write(repo.join("file.txt"), "feature\n").unwrap();
        git_ok(&repo, &["commit", "-am", "feature"]);
        git_ok(&repo, &["checkout", "main"]);
        std::fs::write(repo.join("file.txt"), "main\n").unwrap();
        git_ok(&repo, &["commit", "-am", "main"]);

        assert!(
            !git(&repo, &["merge", "feature", "--no-edit"])
                .status
                .success()
        );
        abort_failed_merge(&repo);

        assert!(
            !git(&repo, &["rev-parse", "-q", "--verify", "MERGE_HEAD"])
                .status
                .success()
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("file.txt")).unwrap(),
            "main\n"
        );
        assert!(git(&repo, &["status", "--porcelain"]).stdout.is_empty());
    }

    #[test]
    fn abort_failed_merge_is_noop_without_merge_in_progress() {
        use crate::team::test_support::{git, init_git_repo};
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "abort-merge-noop");
        abort_failed_merge(&repo);
        assert!(git(&repo, &["status", "--porcelain"]).status.success());
    }

    #[test]
    fn commits_ahead_of_main_error_includes_command_and_intent() {
        let tmp = tempfile::tempdir().unwrap();
        let error = commits_ahead_of_main(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("count commits ahead of main before accepting engineer completion"));
        assert!(error.contains("git rev-list --count main..HEAD"));
    }

    #[test]
    fn files_changed_from_main_error_on_non_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let error = files_changed_from_main(tmp.path()).unwrap_err().to_string();
        assert!(error.contains("narration-only detection"));
    }

    #[test]
    fn files_changed_from_main_zero_when_no_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        // Set up a git repo with main branch
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        // Create branch with empty commit (no file changes)
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "narration only"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert_eq!(diff_stat_from_main(repo).unwrap(), "");
        assert_eq!(files_changed_from_main(repo).unwrap(), 0);
    }

    #[test]
    fn files_changed_from_main_counts_changed_files() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        // Create branch with actual file change
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("src.rs"), "fn main() {}").unwrap();
        std::fs::write(repo.join("test.rs"), "fn test() {}").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "real changes"])
            .current_dir(repo)
            .output()
            .unwrap();
        let diff_stat = diff_stat_from_main(repo).unwrap();
        assert!(diff_stat.contains("src.rs"));
        assert!(diff_stat.contains("test.rs"));
        assert_eq!(files_changed_from_main(repo).unwrap(), 2);
    }

    #[test]
    fn files_changed_from_main_ignores_main_only_changes_after_branch_point() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("task.rs"), "fn task() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "task.rs"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "task change"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("main_only.rs"), "fn main_only() {}\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "main_only.rs"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "main only change"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();

        let diff_stat = diff_stat_from_main(repo).unwrap();
        assert!(diff_stat.contains("task.rs"));
        assert!(!diff_stat.contains("main_only.rs"));
        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
    }

    #[test]
    fn code_files_changed_from_main_ignores_docs_only_diff() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("notes.md"), "narration only").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "docs only"])
            .current_dir(repo)
            .output()
            .unwrap();
        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
        assert_eq!(code_files_changed_from_main(repo).unwrap(), 0);
    }

    #[test]
    fn files_changed_from_main_ignores_runtime_noise_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();

        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::create_dir_all(repo.join(".batty-target").join("debug")).unwrap();
        std::fs::write(repo.join(".batty-target").join("CACHEDIR.TAG"), "cache\n").unwrap();
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::write(repo.join(".batty").join("state.json"), "{}\n").unwrap();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("notes.md"), "docs only\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A", "-f", ".batty-target", ".batty", "docs"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "runtime noise"])
            .current_dir(repo)
            .output()
            .unwrap();

        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
        assert_eq!(code_files_changed_from_main(repo).unwrap(), 0);
    }

    #[test]
    fn code_files_changed_from_main_counts_source_changes_but_skips_runtime_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["checkout", "-b", "main"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "hello").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .output()
            .unwrap();

        std::process::Command::new("git")
            .args(["checkout", "-b", "task-branch"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(repo.join(".batty-target").join("debug")).unwrap();
        std::fs::write(repo.join(".batty-target").join("CACHEDIR.TAG"), "cache\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "-A", "-f", "src", ".batty-target"])
            .current_dir(repo)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "source plus noise"])
            .current_dir(repo)
            .output()
            .unwrap();

        assert_eq!(files_changed_from_main(repo).unwrap(), 1);
        assert_eq!(code_files_changed_from_main(repo).unwrap(), 1);
    }

    #[test]
    fn is_non_code_path_preserves_code_adjacent_files() {
        assert!(!is_non_code_path(Path::new("Cargo.toml")));
        assert!(!is_non_code_path(Path::new("Cargo.lock")));
        assert!(!is_non_code_path(Path::new("src/main.rs")));
        assert!(is_non_code_path(Path::new("docs/guide.md")));
        assert!(is_non_code_path(Path::new("assets/logo.svg")));
    }

    #[test]
    fn completion_noise_paths_are_excluded_from_evidence_counts() {
        assert!(is_completion_noise_path(Path::new(
            ".batty-target/debug/output"
        )));
        assert!(is_completion_noise_path(Path::new(".batty/state.json")));
        assert!(is_completion_noise_path(Path::new("target/debug/batty")));
        assert!(is_completion_noise_path(Path::new("CLAUDE.md")));
        assert!(!is_completion_noise_path(Path::new("docs/guide.md")));
        assert!(!is_completion_noise_path(Path::new("src/main.rs")));
    }

    #[test]
    fn run_git_with_context_bails_on_nonzero_exit() {
        use crate::team::test_support::init_git_repo;
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "strict-runner");
        let err = run_git_with_context(&repo, &["rev-parse", "--verify", "nope"], "resolve nope")
            .unwrap_err()
            .to_string();
        assert!(err.contains("resolve nope"), "{err}");
        assert!(err.contains("rev-parse"), "{err}");

        let output =
            run_git_with_context_allow_fail(&repo, &["rev-parse", "--verify", "nope"], "probe")
                .unwrap();
        assert!(!output.status.success());
        assert!(run_git_with_context(&repo, &["rev-parse", "HEAD"], "head").is_ok());
    }
}
