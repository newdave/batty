//! Multi-repo projects: per-sub-repo diff evidence and the all-or-nothing
//! merge (preflight every sub-repo, merge, roll back on a later failure).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::team::auto_merge;
use crate::team::task_loop::{
    checkout_worktree_branch_from_trunk, current_worktree_branch, engineer_base_branch_name,
};

use super::super::git_ops::{
    abort_failed_merge, code_files_changed_from_trunk, commits_ahead_of_trunk,
    diff_stat_from_trunk, files_changed_from_trunk, rev_parse, revert_out_of_scope_files,
    run_git_with_context, run_git_with_context_allow_fail,
};
use super::super::lock::{MergeMode, MergeOutcome, MergeSuccess};

/// Files changed relative to trunk across the sub-repo worktrees under
/// `worktree_dir`, each path prefixed with its sub-repo name (`A/src/x.rs`).
pub(crate) fn multi_repo_changed_paths(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    for repo_name in sub_repo_names {
        let sub_wt = worktree_dir.join(repo_name);
        if !sub_wt.exists() {
            continue;
        }
        let files =
            crate::team::daemon::verification::changed_files_from_trunk(&sub_wt, trunk_branch)
                .with_context(|| format!("list changed files in sub-repo {repo_name}"))?;
        paths.extend(files.into_iter().map(|file| format!("{repo_name}/{file}")));
    }
    Ok(paths)
}

/// Revert out-of-scope files (paths prefixed with their sub-repo name, as
/// from [`multi_repo_changed_paths`]) in the sub-repo each belongs to.
/// Returns true if any sub-repo changed.
pub(super) fn revert_multi_repo_out_of_scope_files(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    out_of_scope: &[String],
    trunk_branch: &str,
) -> Result<bool> {
    let mut reverted = false;
    for repo_name in sub_repo_names {
        let prefix = format!("{repo_name}/");
        let files: Vec<String> = out_of_scope
            .iter()
            .filter_map(|path| path.strip_prefix(&prefix))
            .map(str::to_string)
            .collect();
        if files.is_empty() {
            continue;
        }
        reverted |= revert_out_of_scope_files(&worktree_dir.join(repo_name), &files, trunk_branch)
            .with_context(|| format!("revert out-of-scope files in sub-repo {repo_name}"))?;
    }
    Ok(reverted)
}

/// Merge engineer branches across all sub-repos in a multi-repo project.
///
/// All-or-nothing: every sub-repo with work is preflighted (commit count,
/// root checkout on trunk and clean, conflict-free merge) before any merge
/// runs, and repos already merged are rolled back if a later one fails. The
/// user's sub-repo checkouts are never switched to another branch. Only the
/// worktrees of repos that were actually merged are reset afterwards.
pub(super) fn merge_multi_repo_engineer_branch(
    project_root: &Path,
    engineer_name: &str,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<MergeOutcome> {
    // Phase 1: preflight every repo before touching any of them.
    let mut candidates = Vec::new();
    for repo_name in sub_repo_names {
        let repo_root = project_root.join(repo_name);
        let sub_wt = project_root
            .join(".batty")
            .join("worktrees")
            .join(engineer_name)
            .join(repo_name);
        if !sub_wt.exists() {
            continue;
        }
        let ahead = commits_ahead_of_trunk(&sub_wt, trunk_branch).with_context(|| {
            format!("count commits to merge in sub-repo {repo_name} before multi-repo merge")
        })?;
        if ahead == 0 {
            continue;
        }
        if let Some(reason) = multi_repo_preflight_failure(&repo_root, &sub_wt, trunk_branch)? {
            return Ok(reason);
        }
        candidates.push((repo_name.as_str(), repo_root, sub_wt));
    }

    // Phase 2: merge, rolling back earlier repos if a later one fails.
    let mut merged: Vec<(&str, PathBuf, String)> = Vec::new();
    for (repo_name, repo_root, sub_wt) in &candidates {
        let trunk_before = rev_parse(repo_root, &format!("refs/heads/{trunk_branch}"))?;
        let outcome =
            match merge_engineer_branch_in_repo(repo_root, sub_wt, engineer_name, trunk_branch) {
                Ok(outcome) => outcome,
                Err(error) => {
                    rollback_multi_repo_merges(&merged, trunk_branch);
                    return Err(error);
                }
            };
        match outcome {
            MergeOutcome::Success(_) => merged.push((repo_name, repo_root.clone(), trunk_before)),
            other => {
                rollback_multi_repo_merges(&merged, trunk_branch);
                return Ok(other);
            }
        }
    }

    // Phase 3: reset only the worktrees whose work landed.
    for (repo_name, repo_root, _) in &merged {
        if let Err(e) =
            reset_engineer_worktree_in_repo(repo_root, engineer_name, repo_name, trunk_branch)
        {
            warn!(
                engineer = engineer_name,
                repo = repo_name,
                error = %e,
                "worktree reset failed after multi-repo merge"
            );
        }
    }

    Ok(MergeOutcome::Success(MergeSuccess {
        mode: MergeMode::DirectRoot,
        reason: None,
    }))
}

/// Same root-checkout rules as the single-repo path, applied before any
/// merge so one bad repo cannot leave the others half-merged. Returns the
/// outcome to report when the repo is not safe to merge into.
fn multi_repo_preflight_failure(
    repo_root: &Path,
    sub_wt: &Path,
    trunk_branch: &str,
) -> Result<Option<MergeOutcome>> {
    let current = current_worktree_branch(repo_root)
        .with_context(|| format!("inspect sub-repo checkout {}", repo_root.display()))?;
    if current != trunk_branch {
        return Ok(Some(MergeOutcome::MergeFailure(format!(
            "sub-repo {} root checkout is on '{current}', not '{trunk_branch}'; refusing to switch the user's checkout",
            repo_root.display()
        ))));
    }
    let dirty = super::super::operations::inspect_root_dirty_state(repo_root)?;
    if !dirty.is_clean() && !dirty.is_runtime_only() {
        return Ok(Some(MergeOutcome::MergeFailure(format!(
            "sub-repo {} root {trunk_branch} checkout has local changes; deferring multi-repo merge",
            repo_root.display()
        ))));
    }
    let branch = current_worktree_branch(sub_wt)?;
    if auto_merge::branch_has_conflicts(repo_root, trunk_branch, &branch) {
        return Ok(Some(MergeOutcome::RebaseConflict(format!(
            "branch '{branch}' conflicts with {trunk_branch} in {} (preflight); no sub-repo was merged",
            repo_root.display()
        ))));
    }
    Ok(None)
}

/// Undo merges already applied in earlier sub-repos. `reset --keep` refuses
/// rather than discarding local changes; it only runs when trunk is still
/// checked out, so the user's checkout is never switched.
fn rollback_multi_repo_merges(merged: &[(&str, PathBuf, String)], trunk_branch: &str) {
    for (repo_name, repo_root, trunk_before) in merged.iter().rev() {
        let result = run_git_with_context(
            repo_root,
            &["reset", "--keep", trunk_before],
            &format!("roll back {trunk_branch} in {repo_name} after multi-repo merge failure"),
        );
        match result {
            Ok(_) => warn!(
                repo = repo_name,
                trunk_before = %trunk_before,
                "rolled back sub-repo merge after a later sub-repo failed"
            ),
            Err(error) => warn!(
                repo = repo_name,
                error = %error,
                "failed to roll back sub-repo merge; manual cleanup needed"
            ),
        }
    }
}

/// Merge a single sub-repo's engineer worktree branch into trunk.
fn merge_engineer_branch_in_repo(
    repo_root: &Path,
    worktree_dir: &Path,
    engineer_name: &str,
    trunk_branch: &str,
) -> Result<MergeOutcome> {
    let branch = current_worktree_branch(worktree_dir)?;
    info!(engineer = engineer_name, branch = %branch, repo = %repo_root.display(), "merging sub-repo worktree branch");

    // Never switch the user's checkout: the caller preflights that the
    // sub-repo root is already on trunk.
    let current_trunk = current_worktree_branch(repo_root)?;
    if current_trunk != trunk_branch {
        return Ok(MergeOutcome::MergeFailure(format!(
            "sub-repo {} root checkout is on '{current_trunk}', not '{trunk_branch}'",
            repo_root.display()
        )));
    }

    let rebase = run_git_with_context_allow_fail(
        worktree_dir,
        &["rebase", trunk_branch],
        &format!(
            "rebase '{branch}' onto {trunk_branch} in {}",
            repo_root.display()
        ),
    )?;
    if !rebase.status.success() {
        let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
        let _ =
            run_git_with_context_allow_fail(worktree_dir, &["rebase", "--abort"], "abort rebase");
        return Ok(MergeOutcome::RebaseConflict(format!(
            "rebase conflict in {}: {stderr}",
            repo_root.display()
        )));
    }

    let output = run_git_with_context_allow_fail(
        repo_root,
        &["merge", &branch, "--no-edit"],
        &format!(
            "merge '{branch}' into {trunk_branch} in {}",
            repo_root.display()
        ),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        abort_failed_merge(repo_root);
        return Ok(MergeOutcome::MergeFailure(format!(
            "merge failed in {}: {stderr}",
            repo_root.display()
        )));
    }

    println!(
        "Merged branch '{branch}' from {engineer_name} in {}",
        repo_root.file_name().unwrap_or_default().to_string_lossy()
    );
    Ok(MergeOutcome::Success(MergeSuccess {
        mode: MergeMode::DirectRoot,
        reason: None,
    }))
}

fn reset_engineer_worktree_in_repo(
    repo_root: &Path,
    engineer_name: &str,
    repo_name: &str,
    trunk_branch: &str,
) -> Result<()> {
    let worktree_dir = repo_root
        .parent()
        .unwrap_or(repo_root)
        .join(".batty")
        .join("worktrees")
        .join(engineer_name)
        .join(repo_name);
    if !worktree_dir.exists() {
        return Ok(());
    }
    let base_branch = engineer_base_branch_name(engineer_name);
    checkout_worktree_branch_from_trunk(&worktree_dir, &base_branch, trunk_branch)?;
    Ok(())
}

/// For multi-repo: sum commits ahead of main across all sub-repo worktrees.
pub(super) fn multi_repo_commits_ahead_of_trunk(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += commits_ahead_of_trunk(&sub_wt, trunk_branch)?;
        }
    }
    Ok(total)
}

/// For multi-repo: get the task branch from the first sub-repo that has commits.
pub(super) fn multi_repo_task_branch(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<String> {
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() && commits_ahead_of_trunk(&sub_wt, trunk_branch)? > 0 {
            return current_worktree_branch(&sub_wt);
        }
    }
    // Fall back to first existing sub-repo
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            return current_worktree_branch(&sub_wt);
        }
    }
    bail!("no sub-repo worktrees found in {}", worktree_dir.display())
}

/// For multi-repo: sum files changed from main across all sub-repo worktrees.
pub(super) fn multi_repo_files_changed_from_trunk(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += files_changed_from_trunk(&sub_wt, trunk_branch).unwrap_or(0);
        }
    }
    Ok(total)
}

pub(super) fn multi_repo_code_files_changed_from_trunk(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<u32> {
    let mut total = 0u32;
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if sub_wt.exists() {
            total += code_files_changed_from_trunk(&sub_wt, trunk_branch).unwrap_or(0);
        }
    }
    Ok(total)
}

pub(super) fn multi_repo_diff_stat_from_trunk(
    worktree_dir: &Path,
    sub_repo_names: &[String],
    trunk_branch: &str,
) -> Result<String> {
    let mut stats = Vec::new();
    for name in sub_repo_names {
        let sub_wt = worktree_dir.join(name);
        if !sub_wt.exists() {
            continue;
        }
        let diff_stat = diff_stat_from_trunk(&sub_wt, trunk_branch).unwrap_or_default();
        if !diff_stat.is_empty() {
            stats.push(format!("[{name}]\n{diff_stat}"));
        }
    }
    Ok(stats.join("\n"))
}
