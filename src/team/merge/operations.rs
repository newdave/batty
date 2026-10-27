//! Core merge and worktree-reset operations.
//!
//! `merge_engineer_branch` rebases an engineer's worktree branch onto trunk and
//! fast-forward merges it. `reset_engineer_worktree` returns the worktree to
//! the engineer's base branch after a successful merge.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::team::task_loop::{
    ADDITIVE_CONFLICT_AUTO_RESOLVE_FENCE, branch_is_merged_into,
    checkout_worktree_branch_from_trunk, current_worktree_branch, delete_branch,
    engineer_base_branch_name, is_worktree_safe_to_mutate, merge_additive_only_text,
    worktree_has_user_changes,
};
use crate::team::verification::{self, VerifyStatus};

use super::git_ops::{
    abort_failed_merge, describe_git_failure, force_clean_worktree, run_git_with_context,
    run_git_with_context_allow_fail,
};
use super::lock::{MergeMode, MergeOutcome, MergeSuccess};

/// The root checkout has trunk checked out with local changes that a
/// fast-forward to the merge would overwrite. Nothing landed; the merge can
/// be retried once the root checkout is clean.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "root {trunk_branch} checkout is dirty and its local changes conflict with the merge; \
     deferring until the root checkout is clean: {stderr}"
)]
pub(crate) struct RootCheckoutDirty {
    pub(crate) trunk_branch: String,
    /// Paths git reported as in the way, from its stderr.
    pub(crate) paths: Vec<String>,
    pub(crate) stderr: String,
}

/// Paths listed (tab-indented) under git's "would be overwritten" errors.
pub(crate) fn overwritten_paths_from_git_stderr(stderr: &str) -> Vec<String> {
    let mut paths: Vec<String> = stderr
        .lines()
        .filter(|line| line.starts_with('\t'))
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RootMergePlan {
    mode: MergeMode,
    reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RootDirtyState {
    pub(crate) staged_paths: Vec<String>,
    pub(crate) unstaged_paths: Vec<String>,
    pub(crate) runtime_paths: Vec<String>,
    pub(crate) source_paths: Vec<String>,
}

impl RootDirtyState {
    pub(crate) fn is_clean(&self) -> bool {
        self.runtime_paths.is_empty() && self.source_paths.is_empty()
    }

    pub(crate) fn is_runtime_only(&self) -> bool {
        !self.is_clean() && self.source_paths.is_empty()
    }
}

pub(crate) fn inspect_root_dirty_state(project_root: &Path) -> Result<RootDirtyState> {
    let staged_paths = git_name_only(project_root, &["diff", "--cached", "--name-only"])?;
    let mut unstaged_paths = git_name_only(project_root, &["diff", "--name-only"])?;
    unstaged_paths.extend(git_name_only(
        project_root,
        &["ls-files", "--others", "--exclude-standard"],
    )?);
    unstaged_paths.sort();
    unstaged_paths.dedup();

    let all_paths = staged_paths
        .iter()
        .chain(unstaged_paths.iter())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut runtime_paths = Vec::new();
    let mut source_paths = Vec::new();
    for path in all_paths {
        if is_batty_managed_runtime_path(&path) {
            runtime_paths.push(path);
        } else {
            source_paths.push(path);
        }
    }

    Ok(RootDirtyState {
        staged_paths,
        unstaged_paths,
        runtime_paths,
        source_paths,
    })
}

fn git_name_only(project_root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = run_git_with_context_allow_fail(
        project_root,
        args,
        &format!("inspect root dirty paths with git {}", args.join(" ")),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "{}",
            describe_git_failure(
                project_root,
                args,
                &format!("inspect root dirty paths with git {}", args.join(" ")),
                &stderr,
            )
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn is_batty_managed_runtime_path(path: &str) -> bool {
    path == ".batty/team_config/board/.lock"
        || path == ".batty/team_config/board/activity.jsonl"
        || path == ".batty/events.jsonl"
        || path == ".batty/telemetry.db"
        || path == ".batty/telemetry.db-shm"
        || path == ".batty/telemetry.db-wal"
        || path == ".omx/notepad.md"
        || path == ".omx/project-memory.json"
        || path.starts_with(".batty/")
        || path.starts_with(".batty-target/")
        || path.starts_with(".omx/logs/")
        || path.starts_with(".omx/state/")
        || path.starts_with("target/")
}

#[cfg(test)]
pub(crate) fn merge_engineer_branch(
    project_root: &Path,
    engineer_name: &str,
) -> Result<MergeOutcome> {
    merge_engineer_branch_into_trunk(project_root, engineer_name, "main")
}

pub(crate) fn merge_engineer_branch_into_trunk(
    project_root: &Path,
    engineer_name: &str,
    trunk_branch: &str,
) -> Result<MergeOutcome> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        bail!(
            "no worktree found for '{}' at {}",
            engineer_name,
            worktree_dir.display()
        );
    }

    let branch = current_worktree_branch(&worktree_dir)?;
    info!(engineer = engineer_name, branch = %branch, "merging worktree branch");

    // Fetch latest trunk into the worktree so rebase sees current state.
    let _ = run_git_with_context_allow_fail(
        &worktree_dir,
        &["fetch", ".", &format!("{trunk_branch}:{trunk_branch}")],
        &format!("fetch {trunk_branch} into worktree before rebase"),
    );

    let rebase = run_git_with_context_allow_fail(
        &worktree_dir,
        &["rebase", trunk_branch],
        &format!(
            "rebase engineer branch '{branch}' onto {trunk_branch} before merging for '{engineer_name}'"
        ),
    )?;

    if !rebase.status.success() {
        // Try to auto-resolve: for auto-generated files (.cargo/config.toml),
        // accept the main version and continue the rebase.
        let resolved = try_auto_resolve_rebase(&worktree_dir);
        if !resolved {
            let stderr = String::from_utf8_lossy(&rebase.stderr).trim().to_string();
            let _ = run_git_with_context_allow_fail(
                &worktree_dir,
                &["rebase", "--abort"],
                &format!("abort rebase for engineer branch '{branch}' after conflict"),
            );
            warn!(engineer = engineer_name, branch = %branch, "rebase conflict during merge");
            return Ok(MergeOutcome::RebaseConflict(describe_git_failure(
                &worktree_dir,
                &["rebase", trunk_branch],
                &format!(
                    "rebase engineer branch '{branch}' onto {trunk_branch} before merging for '{engineer_name}'"
                ),
                &stderr,
            )));
        }
        info!(engineer = engineer_name, branch = %branch, "auto-resolved rebase conflicts");
    }

    let verification =
        verification::verify_project(&worktree_dir, project_root).with_context(|| {
            format!("run verification before merging branch '{branch}' for '{engineer_name}'")
        })?;
    if verification.status == VerifyStatus::Failed {
        let regressions = if verification.regressions.is_empty() {
            "unknown".to_string()
        } else {
            verification.regressions.join(", ")
        };
        let report_path = verification
            .report_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string());
        return Ok(MergeOutcome::MergeFailure(format!(
            "verification detected regressions for behavior(s): {regressions}. Report: {report_path}"
        )));
    }

    let merge_plan = plan_root_merge(project_root, trunk_branch).with_context(|| {
        format!("determine merge strategy for engineer branch '{branch}' from '{engineer_name}'")
    })?;
    let merge_result = match merge_plan.mode {
        MergeMode::DirectRoot => {
            merge_branch_into_root_trunk(project_root, engineer_name, &branch, trunk_branch)
        }
        MergeMode::IsolatedIntegration => merge_branch_via_isolated_integration(
            project_root,
            engineer_name,
            &branch,
            trunk_branch,
        ),
    };
    if let Err(error) = merge_result {
        if error.downcast_ref::<RootCheckoutDirty>().is_some() {
            // Not a merge failure: nothing landed and a retry can succeed.
            return Err(error);
        }
        let prefix = match merge_plan.mode {
            MergeMode::DirectRoot => "direct merge path failed",
            MergeMode::IsolatedIntegration => "isolated merge path failed",
        };
        let reason = merge_plan
            .reason
            .as_deref()
            .map(|reason| format!("{prefix}: {reason}: {error}"))
            .unwrap_or_else(|| format!("{prefix}: {error}"));
        warn!(
            engineer = engineer_name,
            branch = %branch,
            mode = ?merge_plan.mode,
            error = %reason,
            "merge execution failed"
        );
        return Ok(MergeOutcome::MergeFailure(reason));
    }

    println!("Merged branch '{branch}' from {engineer_name}");

    if let Err(error) = reset_engineer_worktree_to_trunk(project_root, engineer_name, trunk_branch)
    {
        warn!(
            engineer = engineer_name,
            error = %error,
            "worktree reset failed after merge"
        );
    }

    Ok(MergeOutcome::Success(MergeSuccess {
        mode: merge_plan.mode,
        reason: merge_plan.reason,
    }))
}

fn plan_root_merge(project_root: &Path, trunk_branch: &str) -> Result<RootMergePlan> {
    let branch = current_worktree_branch(project_root).unwrap_or_else(|_| "HEAD".to_string());
    if branch == trunk_branch {
        let root_dirty = inspect_root_dirty_state(project_root)?;
        if root_dirty.is_runtime_only() {
            return Ok(RootMergePlan {
                mode: MergeMode::DirectRoot,
                reason: Some(format!(
                    "root {trunk_branch} checkout has runtime-only local changes"
                )),
            });
        }
        if root_dirty.is_clean() {
            return Ok(RootMergePlan {
                mode: MergeMode::DirectRoot,
                reason: None,
            });
        }
    }

    let has_user_changes = worktree_has_user_changes(project_root)?;
    let reason = match (branch.as_str(), has_user_changes) {
        (branch, true) if branch == trunk_branch => {
            format!("root {trunk_branch} checkout has local changes")
        }
        ("HEAD", true) => "root checkout is detached HEAD with local changes".to_string(),
        ("HEAD", false) => "root checkout is detached HEAD".to_string(),
        (other, true) => format!("root checkout is on '{other}' with local changes"),
        (other, false) => format!("root checkout is on '{other}' instead of '{trunk_branch}'"),
    };
    Ok(RootMergePlan {
        mode: MergeMode::IsolatedIntegration,
        reason: Some(reason),
    })
}

fn merge_branch_into_root_trunk(
    project_root: &Path,
    engineer_name: &str,
    branch: &str,
    trunk_branch: &str,
) -> Result<()> {
    let output = run_git_with_context_allow_fail(
        project_root,
        &["merge", branch, "--no-edit"],
        &format!("merge engineer branch '{branch}' from '{engineer_name}' into {trunk_branch}"),
    )?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        abort_failed_merge(project_root);
        bail!(
            "{}",
            describe_git_failure(
                project_root,
                &["merge", branch, "--no-edit"],
                &format!(
                    "merge engineer branch '{branch}' from '{engineer_name}' into {trunk_branch}"
                ),
                &stderr,
            )
        );
    }
    Ok(())
}

fn merge_branch_via_isolated_integration(
    project_root: &Path,
    engineer_name: &str,
    branch: &str,
    trunk_branch: &str,
) -> Result<()> {
    let trunk_before = run_git_with_context_allow_fail(
        project_root,
        &["rev-parse", &format!("refs/heads/{trunk_branch}")],
        &format!("capture {trunk_branch} ref before isolated merge"),
    )?;
    if !trunk_before.status.success() {
        bail!(
            "failed to read {trunk_branch} ref before isolated merge: {}",
            String::from_utf8_lossy(&trunk_before.stderr).trim()
        );
    }
    let trunk_before = String::from_utf8_lossy(&trunk_before.stdout)
        .trim()
        .to_string();
    let integration =
        crate::worktree::prepare_integration_worktree(project_root, "merge-main-", trunk_branch)?;
    let merge = run_git_with_context_allow_fail(
        integration.path(),
        &["merge", branch, "--no-edit"],
        &format!(
            "merge engineer branch '{branch}' from '{engineer_name}' in isolated integration worktree"
        ),
    )?;
    if !merge.status.success() {
        let stderr = String::from_utf8_lossy(&merge.stderr).trim().to_string();
        bail!(
            "{}",
            describe_git_failure(
                integration.path(),
                &["merge", branch, "--no-edit"],
                &format!(
                    "merge engineer branch '{branch}' from '{engineer_name}' in isolated integration worktree"
                ),
                &stderr,
            )
        );
    }

    let integration_head = run_git_with_context_allow_fail(
        integration.path(),
        &["rev-parse", "HEAD"],
        "capture isolated integration merge head",
    )?;
    if !integration_head.status.success() {
        bail!(
            "failed to read isolated integration merge head: {}",
            String::from_utf8_lossy(&integration_head.stderr).trim()
        );
    }
    let integration_head = String::from_utf8_lossy(&integration_head.stdout)
        .trim()
        .to_string();

    // If the root checkout has trunk checked out (isolated mode because it is
    // dirty), moving the ref with update-ref would leave root's index and
    // worktree at the old tip: the merge would then show up as staged
    // reversals and a `commit -a` in root would undo it. Fast-forward the
    // checkout instead, which moves ref, index and worktree together and
    // refuses (losing nothing) when the merge touches locally modified files.
    let root_head = run_git_with_context_allow_fail(
        project_root,
        &["symbolic-ref", "-q", "HEAD"],
        "inspect root checkout HEAD before isolated merge",
    )?;
    let root_on_trunk = root_head.status.success()
        && String::from_utf8_lossy(&root_head.stdout).trim()
            == format!("refs/heads/{trunk_branch}");
    if root_on_trunk {
        let ff = run_git_with_context_allow_fail(
            project_root,
            &["merge", "--ff-only", "--no-edit", integration_head.as_str()],
            &format!("fast-forward dirty root {trunk_branch} checkout to isolated merge"),
        )?;
        if !ff.status.success() {
            let stderr = String::from_utf8_lossy(&ff.stderr).trim().to_string();
            return Err(RootCheckoutDirty {
                trunk_branch: trunk_branch.to_string(),
                paths: overwritten_paths_from_git_stderr(&stderr),
                stderr,
            }
            .into());
        }
        return Ok(());
    }

    let advance_trunk = run_git_with_context_allow_fail(
        project_root,
        &[
            "update-ref",
            &format!("refs/heads/{trunk_branch}"),
            integration_head.as_str(),
            trunk_before.as_str(),
        ],
        &format!("advance {trunk_branch} ref after isolated merge"),
    )?;
    if !advance_trunk.status.success() {
        bail!(
            "failed to advance {trunk_branch} after isolated merge: {}",
            String::from_utf8_lossy(&advance_trunk.stderr).trim()
        );
    }
    Ok(())
}

/// Conflicted paths batty may resolve by taking trunk's version. Only
/// daemon-generated files belong here: anything an engineer could
/// legitimately edit (e.g. `src/team/templates/batty_*`) must go through the
/// normal conflict path so their changes are not silently dropped.
fn is_auto_resolvable_managed_file(path: &str) -> bool {
    path == ".cargo/config.toml" || path.ends_with("/.cargo/config.toml")
}

fn list_unmerged_paths(worktree_dir: &Path) -> Option<Vec<String>> {
    let output = run_git_with_context(
        worktree_dir,
        &["diff", "--name-only", "--diff-filter=U"],
        "list rebase conflicts",
    )
    .ok()?;
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// True when `path` is fully resolved in the index (no unmerged stages) and
/// its staged content carries no conflict markers.
fn staged_resolution_is_clean(worktree_dir: &Path, path: &str) -> bool {
    let unmerged = run_git_with_context(
        worktree_dir,
        &["ls-files", "-u", "--", path],
        &format!("check {path} is resolved"),
    );
    match unmerged {
        Ok(output) if output.stdout.is_empty() => {}
        _ => return false,
    }
    let stage_spec = format!(":0:{path}");
    match run_git_with_context(
        worktree_dir,
        &["show", stage_spec.as_str()],
        &format!("read staged {path}"),
    ) {
        Ok(output) => !contains_conflict_markers(&output.stdout),
        Err(_) => false,
    }
}

fn contains_conflict_markers(content: &[u8]) -> bool {
    String::from_utf8_lossy(content).lines().any(|line| {
        // `=======` alone is too common (markdown underlines) to treat as a marker.
        line.starts_with("<<<<<<< ") || line.starts_with(">>>>>>> ")
    })
}

/// Resolve a managed file by taking trunk's version. During a rebase "ours"
/// is the upstream being rebased onto (trunk). Fails (returning false) when
/// trunk has no version — e.g. a delete/modify conflict — or any step fails.
fn resolve_managed_conflict_with_trunk(worktree_dir: &Path, path: &str) -> bool {
    let checkout = run_git_with_context(
        worktree_dir,
        &["checkout", "--ours", "--", path],
        &format!("auto-resolve conflict in {path} (accept trunk)"),
    );
    if let Err(error) = checkout {
        warn!(file = path, error = %error, "could not take trunk version of conflicted file");
        return false;
    }
    if let Err(error) = run_git_with_context(
        worktree_dir,
        &["add", "--", path],
        &format!("stage auto-resolved {path}"),
    ) {
        warn!(file = path, error = %error, "could not stage auto-resolved file");
        return false;
    }
    true
}

/// Resolve every conflicted path, or return false so the caller takes the
/// normal rebase-conflict path.
fn resolve_rebase_conflicts(worktree_dir: &Path, conflicts: &[String]) -> bool {
    for conflict in conflicts {
        let resolved = if is_auto_resolvable_managed_file(conflict) {
            resolve_managed_conflict_with_trunk(worktree_dir, conflict)
        } else if try_auto_resolve_additive_conflict(worktree_dir, conflict) {
            info!(
                file = %conflict,
                "auto-resolved additive-only rebase conflict"
            );
            true
        } else {
            info!(
                file = %conflict,
                "rebase conflict in non-managed file, cannot auto-resolve"
            );
            return false;
        };
        if !resolved || !staged_resolution_is_clean(worktree_dir, conflict) {
            warn!(
                file = %conflict,
                "auto-resolution left the file unresolved or with conflict markers"
            );
            return false;
        }
    }
    true
}

/// Try to auto-resolve rebase conflicts for files that batty manages.
/// Returns true if all conflicts were resolved and the rebase completed.
fn try_auto_resolve_rebase(worktree_dir: &Path) -> bool {
    let Some(conflicts) = list_unmerged_paths(worktree_dir) else {
        return false;
    };
    if conflicts.is_empty() || !resolve_rebase_conflicts(worktree_dir, &conflicts) {
        return false;
    }

    // Continue the rebase — may need multiple rounds if there are stacked conflicts
    for _ in 0..20 {
        let cont = run_git_with_context_allow_fail(
            worktree_dir,
            &["rebase", "--continue"],
            "continue rebase after auto-resolve",
        );
        match cont {
            Ok(output) if output.status.success() => return true,
            Ok(_) => {
                let Some(remaining) = list_unmerged_paths(worktree_dir) else {
                    return false;
                };
                if remaining.is_empty() {
                    continue; // no conflicts, just needs --continue again
                }
                if !resolve_rebase_conflicts(worktree_dir, &remaining) {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    false
}

fn try_auto_resolve_additive_conflict(worktree_dir: &Path, path: &str) -> bool {
    if !ADDITIVE_CONFLICT_AUTO_RESOLVE_FENCE.contains(&path) {
        return false;
    }

    let Some(base) = read_conflict_stage(worktree_dir, 1, path) else {
        return false;
    };
    let Some(current) = read_conflict_stage(worktree_dir, 2, path) else {
        return false;
    };
    let Some(incoming) = read_conflict_stage(worktree_dir, 3, path) else {
        return false;
    };

    let Some(merged) = merge_additive_only_text(&base, &current, &incoming) else {
        return false;
    };

    std::fs::write(worktree_dir.join(path), merged).is_ok()
        && run_git_with_context_allow_fail(
            worktree_dir,
            &["add", path],
            &format!("stage additive-only conflict resolution for {path}"),
        )
        .is_ok_and(|output| output.status.success())
}

fn read_conflict_stage(worktree_dir: &Path, stage: u8, path: &str) -> Option<String> {
    let stage_spec = format!(":{stage}:{path}");
    let output = run_git_with_context_allow_fail(
        worktree_dir,
        &["show", stage_spec.as_str()],
        &format!("read stage {stage} for conflicted path {path}"),
    )
    .ok()?;
    if !output.status.success() || output.stdout.contains(&0) {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
pub(crate) fn reset_engineer_worktree(project_root: &Path, engineer_name: &str) -> Result<()> {
    reset_engineer_worktree_to_trunk(project_root, engineer_name, "main")
}

pub(crate) fn reset_engineer_worktree_to_trunk(
    project_root: &Path,
    engineer_name: &str,
    trunk_branch: &str,
) -> Result<()> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(engineer_name);

    if !worktree_dir.exists() {
        return Ok(());
    }

    let previous_branch = current_worktree_branch(&worktree_dir)?;
    let base_branch = engineer_base_branch_name(engineer_name);

    // Guard: refuse to destroy uncommitted work on a task branch.
    if !is_worktree_safe_to_mutate(&worktree_dir)? {
        warn!(
            engineer = engineer_name,
            worktree = %worktree_dir.display(),
            "skipping worktree reset — uncommitted changes on task branch"
        );
        return Ok(());
    }

    // Force-clean uncommitted changes before switching branches.
    // Without this, `checkout -B` fails when the worktree is dirty.
    force_clean_worktree(&worktree_dir, engineer_name);

    if let Err(error) =
        checkout_worktree_branch_from_trunk(&worktree_dir, &base_branch, trunk_branch)
    {
        warn!(
            engineer = engineer_name,
            current_branch = %previous_branch,
            expected_branch = %base_branch,
            error = %error,
            "failed to reset worktree after merge"
        );
        return Ok(());
    }

    // Verify HEAD landed on the base branch.
    match current_worktree_branch(&worktree_dir) {
        Ok(actual) if actual == base_branch => {}
        Ok(actual) => {
            warn!(
                engineer = engineer_name,
                current_branch = %actual,
                expected_branch = %base_branch,
                "worktree reset did not land on expected branch"
            );
        }
        Err(error) => {
            warn!(
                engineer = engineer_name,
                error = %error,
                "could not verify worktree branch after reset"
            );
        }
    }

    if previous_branch != base_branch
        && previous_branch != "HEAD"
        && crate::worktree::ops::branch_belongs_to_engineer(&previous_branch, engineer_name)
        && branch_is_merged_into(project_root, &previous_branch, trunk_branch)?
        && let Err(error) = delete_branch(project_root, &previous_branch)
    {
        warn!(
            engineer = engineer_name,
            branch = %previous_branch,
            error = %error,
            "failed to delete merged engineer task branch"
        );
    }

    info!(
        engineer = engineer_name,
        branch = %base_branch,
        worktree = %worktree_dir.display(),
        "reset worktree to trunk after merge"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::task_loop::{
        engineer_base_branch_name, prepare_engineer_assignment_worktree, setup_engineer_worktree,
    };
    use crate::team::test_support::{git, git_ok, git_stdout, init_git_repo};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn engineer_worktree_paths(repo: &Path, engineer: &str) -> (PathBuf, PathBuf) {
        let worktree_dir = repo.join(".batty").join("worktrees").join(engineer);
        let team_config_dir = repo.join(".batty").join("team_config");
        (worktree_dir, team_config_dir)
    }

    fn write_script(path: &Path, lines: &[&str]) {
        let body = format!("#!/bin/sh\nprintf '%s\\n' {}\n", lines.join(" "));
        std::fs::write(path, body).unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn overwritten_paths_are_parsed_from_git_stderr() {
        let stderr = "error: Your local changes to the following files would be overwritten by merge:\n\tsrc/lib.rs\n\tCargo.toml\nPlease commit your changes or stash them before you merge.\nerror: The following untracked working tree files would be overwritten by merge:\n\tnew.txt\nAborting";
        assert_eq!(
            overwritten_paths_from_git_stderr(stderr),
            vec!["Cargo.toml", "new.txt", "src/lib.rs"]
        );
        assert!(overwritten_paths_from_git_stderr("fatal: boom").is_empty());
    }

    #[test]
    fn merge_rejects_missing_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let err = merge_engineer_branch(tmp.path(), "eng-1-1").unwrap_err();
        assert!(err.to_string().contains("no worktree found"));
    }

    #[test]
    fn root_dirty_state_blocks_staged_source_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "root-dirty-staged");
        std::fs::write(repo.join("src").join("lib.rs"), "pub fn changed() {}\n").unwrap();
        git_ok(&repo, &["add", "src/lib.rs"]);

        let state = inspect_root_dirty_state(&repo).unwrap();

        assert_eq!(state.staged_paths, vec!["src/lib.rs"]);
        assert_eq!(state.source_paths, vec!["src/lib.rs"]);
        assert!(state.runtime_paths.is_empty());
    }

    #[test]
    fn root_dirty_state_blocks_unstaged_source_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "root-dirty-unstaged");
        std::fs::write(repo.join("src").join("lib.rs"), "pub fn changed() {}\n").unwrap();

        let state = inspect_root_dirty_state(&repo).unwrap();

        assert_eq!(state.unstaged_paths, vec!["src/lib.rs"]);
        assert_eq!(state.source_paths, vec!["src/lib.rs"]);
        assert!(state.runtime_paths.is_empty());
    }

    #[test]
    fn root_dirty_state_separates_mixed_source_and_runtime_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "root-dirty-mixed");
        std::fs::write(repo.join("src").join("lib.rs"), "pub fn changed() {}\n").unwrap();
        git_ok(&repo, &["add", "src/lib.rs"]);
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("recovery.md"), "manual recovery\n").unwrap();
        let runtime_dir = repo.join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        std::fs::write(runtime_dir.join("activity.jsonl"), "{\"event\":\"tick\"}\n").unwrap();

        let state = inspect_root_dirty_state(&repo).unwrap();

        assert_eq!(state.staged_paths, vec!["src/lib.rs"]);
        assert_eq!(
            state.unstaged_paths,
            vec![
                ".batty/team_config/board/activity.jsonl",
                "docs/recovery.md"
            ]
        );
        assert_eq!(state.source_paths, vec!["docs/recovery.md", "src/lib.rs"]);
        assert_eq!(
            state.runtime_paths,
            vec![".batty/team_config/board/activity.jsonl"]
        );
    }

    #[test]
    fn root_dirty_state_allows_runtime_only_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "root-dirty-runtime");
        let activity = repo.join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(&activity).unwrap();
        std::fs::write(activity.join("activity.jsonl"), "{\"event\":\"tick\"}\n").unwrap();

        let state = inspect_root_dirty_state(&repo).unwrap();

        assert!(state.is_runtime_only());
        assert_eq!(
            state.runtime_paths,
            vec![".batty/team_config/board/activity.jsonl"]
        );
        assert!(state.source_paths.is_empty());
    }

    #[test]
    fn merge_with_rebase_picks_up_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        std::fs::write(repo.join("other.txt"), "main work\n").unwrap();
        git_ok(&repo, &["add", "other.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(
            result,
            MergeOutcome::Success(MergeSuccess {
                mode: MergeMode::DirectRoot,
                ..
            })
        ));
        assert!(repo.join("feature.txt").exists());
        assert!(repo.join("other.txt").exists());
    }

    #[test]
    fn merge_blocks_when_verification_detects_regression() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(
            repo.join("PARITY.md"),
            r#"---
project: trivial
target: trivial.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: 2026-04-05
overall_parity: 100%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Screen fill | complete | complete | complete | PASS | previous |
"#,
        )
        .unwrap();
        std::fs::write(
            repo.join(".batty/verification.yml"),
            r#"behaviors:
  - behavior: Screen fill
    baseline: scripts/baseline.sh
    candidate: scripts/candidate.sh
    inputs: []
"#,
        )
        .unwrap();
        write_script(&repo.join("scripts/baseline.sh"), &["frame-a", "frame-b"]);
        write_script(&repo.join("scripts/candidate.sh"), &["frame-a", "frame-b"]);
        git_ok(
            &repo,
            &["add", "PARITY.md", ".batty/verification.yml", "scripts"],
        );
        git_ok(&repo, &["commit", "-m", "add verification fixtures"]);

        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-verify");
        setup_engineer_worktree(&repo, &worktree_dir, "eng-verify", &team_config_dir).unwrap();

        write_script(
            &worktree_dir.join("scripts/candidate.sh"),
            &["frame-a", "frame-x"],
        );
        git_ok(&worktree_dir, &["add", "scripts/candidate.sh"]);
        git_ok(&worktree_dir, &["commit", "-m", "introduce regression"]);

        let result = merge_engineer_branch(&repo, "eng-verify").unwrap();
        match result {
            MergeOutcome::MergeFailure(message) => {
                assert!(message.contains("verification detected regressions"));
                assert!(message.contains("Screen fill"));
            }
            other => panic!("expected verification failure, got {other:?}"),
        }
        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );
        assert_eq!(
            git_stdout(&repo, &["show", "main:scripts/candidate.sh"]),
            "#!/bin/sh\nprintf '%s\\n' frame-a frame-b"
        );
    }

    #[test]
    fn reset_worktree_after_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success(_)));

        let main_head = git_stdout(&repo, &["rev-parse", "HEAD"]);
        let worktree_head = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        assert_eq!(main_head, worktree_head);
    }

    #[test]
    fn merge_empty_diff_returns_success() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-empty");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-empty", &team_config_dir).unwrap();
        let main_before = git_stdout(&repo, &["rev-parse", "main"]);

        let result = merge_engineer_branch(&repo, "eng-empty").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
        assert_eq!(git_stdout(&repo, &["rev-parse", "main"]), main_before);
    }

    #[test]
    fn merge_empty_diff_resets_worktree_to_engineer_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-empty");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-empty", &team_config_dir).unwrap();

        let result = merge_engineer_branch(&repo, "eng-empty").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-empty")
        );
    }

    #[test]
    fn merge_with_two_main_advances_rebases_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-stale");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-stale", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        std::fs::write(repo.join("main-one.txt"), "main one\n").unwrap();
        git_ok(&repo, &["add", "main-one.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance 1"]);

        std::fs::write(repo.join("main-two.txt"), "main two\n").unwrap();
        git_ok(&repo, &["add", "main-two.txt"]);
        git_ok(&repo, &["commit", "-m", "main advance 2"]);

        let result = merge_engineer_branch(&repo, "eng-stale").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
        assert!(repo.join("feature.txt").exists());
        assert!(repo.join("main-one.txt").exists());
        assert!(repo.join("main-two.txt").exists());
    }

    #[test]
    fn reset_worktree_restores_engineer_base_branch_after_task_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-1",
            "eng-1/42",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success(_)));
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-1")
        );

        let branch_check = git(&repo, &["rev-parse", "--verify", "eng-1/42"]);
        assert!(
            !branch_check.status.success(),
            "merged task branch should be deleted"
        );
    }

    #[test]
    fn reset_worktree_leaves_clean_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-1");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("new.txt"), "content\n").unwrap();
        git_ok(&worktree_dir, &["add", "new.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "add file"]);

        let result = merge_engineer_branch(&repo, "eng-1").unwrap();
        assert!(matches!(result, MergeOutcome::Success(_)));

        let status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        let tracked_changes: Vec<&str> = status
            .lines()
            .filter(|line| !line.starts_with("?? .batty/") && !line.starts_with("?? .cargo/"))
            .collect();
        assert!(
            tracked_changes.is_empty(),
            "worktree has tracked changes: {:?}",
            tracked_changes
        );
    }

    #[test]
    fn reset_worktree_noops_when_worktree_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");

        reset_engineer_worktree(&repo, "eng-missing").unwrap();
    }

    #[test]
    fn reset_worktree_keeps_unmerged_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-keep");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-keep",
            "eng-keep/77",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "keep me\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "unmerged feature"]);

        reset_engineer_worktree(&repo, "eng-keep").unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-keep")
        );
        assert!(
            git(&repo, &["rev-parse", "--verify", "eng-keep/77"])
                .status
                .success()
        );
    }

    #[test]
    fn reset_worktree_deletes_merged_legacy_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-legacy");

        setup_engineer_worktree(
            &repo,
            &worktree_dir,
            &engineer_base_branch_name("eng-legacy"),
            &team_config_dir,
        )
        .unwrap();
        git_ok(
            &worktree_dir,
            &["checkout", "-B", "eng-legacy/task-55", "main"],
        );
        std::fs::write(worktree_dir.join("legacy.txt"), "legacy branch work\n").unwrap();
        git_ok(&worktree_dir, &["add", "legacy.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "legacy task work"]);
        git_ok(&repo, &["merge", "eng-legacy/task-55", "--no-edit"]);

        reset_engineer_worktree(&repo, "eng-legacy").unwrap();

        assert!(
            !git(&repo, &["rev-parse", "--verify", "eng-legacy/task-55"])
                .status
                .success()
        );
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-legacy")
        );
    }

    #[test]
    fn reset_worktree_keeps_non_engineer_namespace_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-keep");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-keep", &team_config_dir).unwrap();
        git_ok(&worktree_dir, &["checkout", "-B", "feature/custom", "main"]);
        std::fs::write(worktree_dir.join("feature.txt"), "non engineer branch\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "feature branch work"]);

        reset_engineer_worktree(&repo, "eng-keep").unwrap();

        assert!(
            git(&repo, &["rev-parse", "--verify", "feature/custom"])
                .status
                .success()
        );
    }

    #[test]
    fn merge_success_deletes_merged_engineer_branch_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-delete");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-delete", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "remove branch\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        let result = merge_engineer_branch(&repo, "eng-delete").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
        assert!(
            !git(&repo, &["rev-parse", "--verify", "eng-delete"])
                .status
                .success()
        );
    }

    #[test]
    fn merge_rebase_conflict_returns_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-2");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("conflict.txt"), "original\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "add conflict file"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("conflict.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "conflict.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(repo.join("conflict.txt"), "main version\n").unwrap();
        git_ok(&repo, &["add", "conflict.txt"]);
        git_ok(&repo, &["commit", "-m", "main change"]);

        let result = merge_engineer_branch(&repo, "eng-2").unwrap();
        assert!(matches!(result, MergeOutcome::RebaseConflict(_)));

        let status = git(&worktree_dir, &["status", "--porcelain"]);
        assert!(status.status.success());
    }

    #[test]
    fn merge_rebase_additive_conflict_keeps_both_sides() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-2");
        let review_file = repo.join("src").join("team").join("review.rs");

        std::fs::create_dir_all(review_file.parent().unwrap()).unwrap();
        std::fs::write(
            &review_file,
            "const REVIEW_CHECKS: &[&str] = &[\n    \"existing\",\n];\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "src/team/review.rs"]);
        git_ok(&repo, &["commit", "-m", "add review checks"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join("src").join("team").join("review.rs"),
            "const REVIEW_CHECKS: &[&str] = &[\n    \"engineer\",\n    \"existing\",\n];\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", "src/team/review.rs"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer review addition"]);

        std::fs::write(
            &review_file,
            "const REVIEW_CHECKS: &[&str] = &[\n    \"main\",\n    \"existing\",\n];\n",
        )
        .unwrap();
        git_ok(&repo, &["add", "src/team/review.rs"]);
        git_ok(&repo, &["commit", "-m", "main review addition"]);

        let result = merge_engineer_branch(&repo, "eng-2").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)));
        let merged = std::fs::read_to_string(review_file).unwrap();
        assert!(merged.contains("\"main\""));
        assert!(merged.contains("\"engineer\""));
        assert!(merged.contains("\"existing\""));
    }

    fn managed_config_conflict_keeps_trunk_version(engineer_only_touches_config: bool) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-2");
        let config = repo.join(".cargo").join("config.toml");

        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, "[build]\njobs = 1\n").unwrap();
        git_ok(&repo, &["add", ".cargo/config.toml"]);
        git_ok(&repo, &["commit", "-m", "add cargo config"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-2", &team_config_dir).unwrap();

        std::fs::write(
            worktree_dir.join(".cargo").join("config.toml"),
            "[build]\njobs = 99\n",
        )
        .unwrap();
        git_ok(&worktree_dir, &["add", ".cargo/config.toml"]);
        if !engineer_only_touches_config {
            std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
            git_ok(&worktree_dir, &["add", "feature.txt"]);
        }
        git_ok(&worktree_dir, &["commit", "-m", "engineer change"]);

        std::fs::write(&config, "[build]\njobs = 4\n").unwrap();
        git_ok(&repo, &["add", ".cargo/config.toml"]);
        git_ok(&repo, &["commit", "-m", "main config change"]);

        let result = merge_engineer_branch(&repo, "eng-2").unwrap();

        assert!(matches!(result, MergeOutcome::Success(_)), "{result:?}");
        let merged =
            String::from_utf8(git(&repo, &["show", "main:.cargo/config.toml"]).stdout).unwrap();
        assert_eq!(merged, "[build]\njobs = 4\n");
        if !engineer_only_touches_config {
            assert!(
                git(&repo, &["cat-file", "-e", "main:feature.txt"])
                    .status
                    .success()
            );
        }
    }

    #[test]
    fn merge_rebase_managed_conflict_keeps_trunk_version() {
        managed_config_conflict_keeps_trunk_version(false);
    }

    #[test]
    fn merge_rebase_managed_conflict_only_change_keeps_trunk_version() {
        managed_config_conflict_keeps_trunk_version(true);
    }

    fn setup_rebase_conflict_repo(
        engineer: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
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

    #[test]
    fn merge_rebase_conflict_aborts_rebase_state() {
        let (_tmp, repo, worktree_dir, _team_config_dir) = setup_rebase_conflict_repo("eng-4");

        let result = merge_engineer_branch(&repo, "eng-4").unwrap();

        assert!(matches!(result, MergeOutcome::RebaseConflict(_)));
        assert!(
            !git(&worktree_dir, &["rev-parse", "--verify", "REBASE_HEAD"])
                .status
                .success()
        );
    }

    #[test]
    fn merge_with_dirty_main_uses_isolated_integration_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-3");
        let team_config_dir = repo.join(".batty").join("team_config");

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();

        let result = merge_engineer_branch(&repo, "eng-3").unwrap();
        match result {
            MergeOutcome::Success(success) => {
                assert_eq!(success.mode, MergeMode::IsolatedIntegration);
                assert!(
                    success
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("local changes")),
                    "expected dirty-root isolation reason, got {success:?}"
                );
                assert_eq!(
                    git_stdout(&repo, &["show", "main:feature.txt"]),
                    "engineer version"
                );
                assert_eq!(
                    std::fs::read_to_string(repo.join("journal.md")).unwrap(),
                    "dirty main\n"
                );
            }
            other => panic!("expected isolated merge success, got {other:?}"),
        }
    }

    #[test]
    fn merge_with_runtime_only_dirty_main_uses_direct_root_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let worktree_dir = repo.join(".batty").join("worktrees").join("eng-runtime");
        let team_config_dir = repo.join(".batty").join("team_config");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-runtime", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        let activity_dir = repo.join(".batty").join("team_config").join("board");
        std::fs::create_dir_all(&activity_dir).unwrap();
        std::fs::write(
            activity_dir.join("activity.jsonl"),
            "{\"event\":\"tick\"}\n",
        )
        .unwrap();

        let result = merge_engineer_branch(&repo, "eng-runtime").unwrap();
        match result {
            MergeOutcome::Success(success) => {
                assert_eq!(success.mode, MergeMode::DirectRoot);
                assert!(
                    success
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("runtime-only local changes")),
                    "expected runtime-only direct-root reason, got {success:?}"
                );
                assert_eq!(
                    git_stdout(&repo, &["show", "main:feature.txt"]),
                    "engineer version"
                );
                assert_eq!(
                    std::fs::read_to_string(activity_dir.join("activity.jsonl")).unwrap(),
                    "{\"event\":\"tick\"}\n"
                );
            }
            other => panic!("expected direct merge success, got {other:?}"),
        }
    }

    #[test]
    fn merge_failure_retains_engineer_branch_for_manual_recovery() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-3");

        std::fs::write(repo.join("journal.md"), "base\n").unwrap();
        git_ok(&repo, &["add", "journal.md"]);
        git_ok(&repo, &["commit", "-m", "add journal"]);

        setup_engineer_worktree(&repo, &worktree_dir, "eng-3", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("journal.md"), "engineer version\n").unwrap();
        git_ok(&worktree_dir, &["add", "journal.md"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer update"]);

        std::fs::write(repo.join("journal.md"), "dirty main\n").unwrap();
        std::fs::create_dir_all(repo.join(".batty")).unwrap();
        std::fs::write(
            repo.join(".batty").join("integration-worktrees"),
            "not a directory\n",
        )
        .unwrap();

        let result = merge_engineer_branch(&repo, "eng-3").unwrap();

        assert!(matches!(result, MergeOutcome::MergeFailure(_)));
        assert_eq!(current_worktree_branch(&worktree_dir).unwrap(), "eng-3");
        assert!(
            git(&repo, &["rev-parse", "--verify", "eng-3"])
                .status
                .success()
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("journal.md")).unwrap(),
            "dirty main\n"
        );
        assert!(
            !repo.join("feature.txt").exists(),
            "main ref should not advance when isolated merge preparation fails"
        );
    }

    #[test]
    fn reset_clears_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-reset");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-reset",
            "eng-reset/task-99",
            &team_config_dir,
        )
        .unwrap();

        std::fs::write(worktree_dir.join("done.txt"), "work done\n").unwrap();
        git_ok(&worktree_dir, &["add", "done.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "task work"]);

        // Merge the task branch into main so it's considered merged.
        git_ok(&repo, &["merge", "eng-reset/task-99", "--no-edit"]);

        reset_engineer_worktree(&repo, "eng-reset").unwrap();

        // Verify on base branch.
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-reset")
        );
        // Verify task branch is deleted.
        assert!(
            !git(&repo, &["rev-parse", "--verify", "eng-reset/task-99"])
                .status
                .success(),
            "merged task branch should have been deleted"
        );
    }

    #[test]
    fn reset_handles_uncommitted_changes_on_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-dirty");
        let base = engineer_base_branch_name("eng-dirty");

        // Set up worktree on the base branch (not a task branch).
        setup_engineer_worktree(&repo, &worktree_dir, &base, &team_config_dir).unwrap();

        // Leave uncommitted staged and unstaged changes.
        std::fs::write(worktree_dir.join("staged.txt"), "staged\n").unwrap();
        git_ok(&worktree_dir, &["add", "staged.txt"]);
        std::fs::write(worktree_dir.join("unstaged.txt"), "unstaged\n").unwrap();

        // Reset should succeed — base branch is safe to mutate even when dirty.
        reset_engineer_worktree(&repo, "eng-dirty").unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            base
        );
        // Worktree should be clean after reset.
        let status = git_stdout(&worktree_dir, &["status", "--porcelain"]);
        let tracked_changes: Vec<&str> = status
            .lines()
            .filter(|line| !line.starts_with("?? .batty/") && !line.starts_with("?? .cargo/"))
            .collect();
        assert!(
            tracked_changes.is_empty(),
            "worktree should be clean after reset, got: {:?}",
            tracked_changes
        );
    }

    #[test]
    fn reset_skips_when_dirty_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-dirty-task");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-dirty-task",
            "eng-dirty-task/task-88",
            &team_config_dir,
        )
        .unwrap();

        // Leave uncommitted staged changes on a task branch.
        std::fs::write(worktree_dir.join("staged.txt"), "staged\n").unwrap();
        git_ok(&worktree_dir, &["add", "staged.txt"]);

        // Reset should skip — worktree is dirty on a task branch.
        reset_engineer_worktree(&repo, "eng-dirty-task").unwrap();

        // Worktree should remain on the task branch with changes intact.
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "eng-dirty-task/task-88"
        );
        assert!(worktree_dir.join("staged.txt").exists());
    }

    #[test]
    fn reset_handles_detached_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-detach");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-detach", &team_config_dir).unwrap();

        // Create a commit and detach HEAD.
        std::fs::write(worktree_dir.join("file.txt"), "content\n").unwrap();
        git_ok(&worktree_dir, &["add", "file.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "a commit"]);
        let commit_sha = git_stdout(&worktree_dir, &["rev-parse", "HEAD"]);
        git_ok(&worktree_dir, &["checkout", &commit_sha]);

        // Verify we are in detached HEAD state.
        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD"
        );

        // Reset should still check out the base branch.
        reset_engineer_worktree(&repo, "eng-detach").unwrap();

        assert_eq!(
            git_stdout(&worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            engineer_base_branch_name("eng-detach")
        );
    }

    #[test]
    fn merge_fails_when_project_root_not_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-off");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-off", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "engineer work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer feature"]);

        // Move project root off main onto a detached HEAD.
        git_ok(&repo, &["checkout", "--detach", "HEAD"]);

        let result = merge_engineer_branch(&repo, "eng-off").unwrap();
        match result {
            MergeOutcome::Success(success) => {
                assert_eq!(success.mode, MergeMode::IsolatedIntegration);
                assert!(
                    success
                        .reason
                        .as_deref()
                        .is_some_and(|reason| reason.contains("detached HEAD")),
                    "expected detached-head isolation reason, got {success:?}"
                );
                let branch = git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
                assert_eq!(branch, "HEAD");
            }
            other => panic!("expected isolated merge success, got {other:?}"),
        }
    }

    #[test]
    fn merge_succeeds_when_project_root_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-ok");

        setup_engineer_worktree(&repo, &worktree_dir, "eng-ok", &team_config_dir).unwrap();

        std::fs::write(worktree_dir.join("feature.txt"), "work\n").unwrap();
        git_ok(&worktree_dir, &["add", "feature.txt"]);
        git_ok(&worktree_dir, &["commit", "-m", "engineer work"]);

        assert_eq!(
            git_stdout(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "main"
        );

        let result = merge_engineer_branch(&repo, "eng-ok").unwrap();
        assert!(matches!(
            result,
            MergeOutcome::Success(MergeSuccess {
                mode: MergeMode::DirectRoot,
                ..
            })
        ));
        assert!(repo.join("feature.txt").exists());
    }

    #[test]
    fn reset_worktree_skips_when_dirty_task_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "batty-merge-test");
        let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-wip");

        prepare_engineer_assignment_worktree(
            &repo,
            &worktree_dir,
            "eng-wip",
            "eng-wip/88",
            &team_config_dir,
        )
        .unwrap();

        // Create uncommitted changes on the task branch.
        std::fs::write(worktree_dir.join("wip.txt"), "work in progress\n").unwrap();
        git_ok(&worktree_dir, &["add", "wip.txt"]);

        // reset_engineer_worktree should skip (not error) when dirty on task branch.
        reset_engineer_worktree(&repo, "eng-wip").unwrap();

        // Verify the worktree was NOT reset — still on task branch with changes.
        let branch = current_worktree_branch(&worktree_dir).unwrap();
        assert_eq!(branch, "eng-wip/88");
        assert!(worktree_dir.join("wip.txt").exists());
    }

    // --- isolated merge with trunk checked out in root ---------------------------

    fn status_without_batty(repo: &Path) -> String {
        git_stdout(
            repo,
            &["status", "--porcelain", "--", ".", ":(exclude).batty"],
        )
    }

    #[test]
    fn isolated_merge_moves_dirty_root_checkout_with_trunk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "isolated-dirty-root");
        git_ok(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("feat.txt"), "feature\n").unwrap();
        git_ok(&repo, &["add", "feat.txt"]);
        git_ok(&repo, &["commit", "-q", "-m", "feature"]);
        git_ok(&repo, &["checkout", "-q", "main"]);
        std::fs::write(repo.join("Cargo.toml"), "# user edit\n").unwrap();
        assert_eq!(
            plan_root_merge(&repo, "main").unwrap().mode,
            MergeMode::IsolatedIntegration
        );

        merge_branch_via_isolated_integration(&repo, "eng-1", "feature", "main").unwrap();

        // Ref, index and worktree moved together; the user's edit survives and
        // is the only local change (no phantom reversal of the merge).
        assert!(repo.join("feat.txt").exists());
        assert_eq!(status_without_batty(&repo), "M Cargo.toml");
        assert_eq!(
            std::fs::read_to_string(repo.join("Cargo.toml")).unwrap(),
            "# user edit\n"
        );
        git_ok(&repo, &["merge-base", "--is-ancestor", "feature", "main"]);
    }

    #[test]
    fn isolated_merge_defers_when_root_changes_overlap_merge() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "isolated-overlap");
        git_ok(&repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(repo.join("Cargo.toml"), "# engineer edit\n").unwrap();
        git_ok(&repo, &["commit", "-q", "-am", "feature"]);
        git_ok(&repo, &["checkout", "-q", "main"]);
        let main_before = git_stdout(&repo, &["rev-parse", "main"]);
        std::fs::write(repo.join("Cargo.toml"), "# user edit\n").unwrap();

        let error =
            merge_branch_via_isolated_integration(&repo, "eng-1", "feature", "main").unwrap_err();
        let err = error.to_string();

        assert!(err.contains("deferring"), "{err}");
        let dirty = error
            .downcast_ref::<RootCheckoutDirty>()
            .expect("an overlapping dirty root is a typed, retryable deferral");
        assert_eq!(dirty.paths, vec!["Cargo.toml".to_string()]);
        assert_eq!(git_stdout(&repo, &["rev-parse", "main"]), main_before);
        assert_eq!(
            std::fs::read_to_string(repo.join("Cargo.toml")).unwrap(),
            "# user edit\n"
        );

        // Once the root checkout is clean, the retry lands.
        git_ok(&repo, &["checkout", "--", "Cargo.toml"]);
        merge_branch_via_isolated_integration(&repo, "eng-1", "feature", "main").unwrap();
        git_ok(&repo, &["merge-base", "--is-ancestor", "feature", "main"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("Cargo.toml")).unwrap(),
            "# engineer edit\n"
        );
    }

    // --- rebase auto-resolve -------------------------------------------------------

    /// Commit `path` on main, edit it on `feature` and (unless `delete_on_main`)
    /// edit it differently on main, then start a conflicting rebase.
    fn start_conflicting_rebase(repo: &Path, path: &str, delete_on_main: bool) {
        let file = repo.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "base\n").unwrap();
        git_ok(repo, &["add", path]);
        git_ok(repo, &["commit", "-q", "-m", "add file"]);
        git_ok(repo, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(&file, "engineer\n").unwrap();
        git_ok(repo, &["commit", "-q", "-am", "engineer edit"]);
        git_ok(repo, &["checkout", "-q", "main"]);
        if delete_on_main {
            git_ok(repo, &["rm", "-q", path]);
        } else {
            std::fs::write(&file, "trunk\n").unwrap();
            git_ok(repo, &["add", path]);
        }
        git_ok(repo, &["commit", "-q", "-m", "trunk edit"]);
        git_ok(repo, &["checkout", "-q", "feature"]);
        assert!(!git(repo, &["rebase", "main"]).status.success());
    }

    #[test]
    fn auto_resolve_takes_trunk_for_managed_cargo_config() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto-resolve-cargo");
        start_conflicting_rebase(&repo, ".cargo/config.toml", false);
        assert!(try_auto_resolve_rebase(&repo));
        assert_eq!(
            std::fs::read_to_string(repo.join(".cargo/config.toml")).unwrap(),
            "trunk\n"
        );
    }

    #[test]
    fn auto_resolve_refuses_delete_modify_conflicts() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto-resolve-delete");
        start_conflicting_rebase(&repo, ".cargo/config.toml", true);
        assert!(!try_auto_resolve_rebase(&repo));
        let staged = git(&repo, &["show", ":0:.cargo/config.toml"]);
        assert!(
            !staged.status.success() || !contains_conflict_markers(&staged.stdout),
            "conflict markers must never be staged"
        );
        git_ok(&repo, &["rebase", "--abort"]);
    }

    #[test]
    fn auto_resolve_no_longer_drops_template_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "auto-resolve-template");
        start_conflicting_rebase(&repo, "src/team/templates/batty_engineer.md", false);
        assert!(!try_auto_resolve_rebase(&repo));
        git_ok(&repo, &["rebase", "--abort"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("src/team/templates/batty_engineer.md")).unwrap(),
            "engineer\n"
        );
    }

    #[test]
    fn conflict_marker_detection() {
        assert!(contains_conflict_markers(
            b"a\n<<<<<<< HEAD\nb\n=======\nc\n>>>>>>> x\n"
        ));
        assert!(!contains_conflict_markers(b"Title\n=======\nbody\n"));
    }
}
