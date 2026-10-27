//! Git worktree lifecycle for isolated phase runs.
//!
//! Each `batty work <phase>` run gets a dedicated branch/worktree:
//! `<phase-slug>-run-<NNN>`.
//! The executor runs in that worktree. Cleanup is merge-aware:
//! - merged runs are removed (worktree + branch)
//! - rejected/failed/unmerged runs are retained for inspection

pub mod ops;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) use ops::{
    abort_in_progress_git_operations, archive_branch_if_commits_ahead, heal_stray_task_branch,
    prepare_worktree_for_reset,
};
#[cfg(test)]
use ops::{
    checkout_base_branch_across_repos, repos_for_mutation as iter_repos_for_mutation,
    snapshot_dirty_worktree_to_ref,
};
use ops::{ref_exists, rev_count};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct PhaseWorktree {
    pub repo_root: PathBuf,
    pub base_branch: String,
    pub start_commit: String,
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AgentWorktree {
    pub branch: String,
    pub path: PathBuf,
}

#[derive(Debug)]
pub struct IntegrationWorktree {
    repo_root: PathBuf,
    path: PathBuf,
}

impl IntegrationWorktree {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IntegrationWorktree {
    fn drop(&mut self) {
        let path = self.path.to_string_lossy().into_owned();
        let _ = run_git(
            &self.repo_root,
            ["worktree", "remove", "--force", path.as_str()],
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainStartRefSelection {
    pub ref_name: String,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineBranchRepair {
    pub branch: String,
    pub start_ref: String,
    pub start_commit: String,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchBranchReset {
    pub changed: bool,
    pub start_ref: String,
    pub fallback_reason: Option<String>,
    pub reset_reason: Option<WorktreeResetReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    Failed,
    DryRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupDecision {
    Cleaned,
    KeptForReview,
    KeptForFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreserveFailureMode {
    SkipReset,
    ForceReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeResetReason {
    PreservedBeforeReset,
    CleanReset,
    PreserveFailedResetSkipped,
    PreserveFailedForceReset,
}

impl WorktreeResetReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreservedBeforeReset => "preserved_before_reset",
            Self::CleanReset => "clean_reset",
            Self::PreserveFailedResetSkipped => "preserve_failed_reset_skipped",
            Self::PreserveFailedForceReset => "preserve_failed_force_reset",
        }
    }

    pub fn reset_performed(self) -> bool {
        self != Self::PreserveFailedResetSkipped
    }
}

impl PhaseWorktree {
    pub fn finalize(&self, outcome: RunOutcome) -> Result<CleanupDecision> {
        match outcome {
            RunOutcome::Failed => Ok(CleanupDecision::KeptForFailure),
            RunOutcome::DryRun => {
                remove_worktree(&self.repo_root, &self.path)?;
                delete_branch(&self.repo_root, &self.branch)?;
                Ok(CleanupDecision::Cleaned)
            }
            RunOutcome::Completed => {
                let branch_tip = current_commit(&self.repo_root, &self.branch)?;
                if branch_tip == self.start_commit {
                    return Ok(CleanupDecision::KeptForReview);
                }

                if is_merged_into_base(&self.repo_root, &self.branch, &self.base_branch)? {
                    remove_worktree(&self.repo_root, &self.path)?;
                    delete_branch(&self.repo_root, &self.branch)?;
                    Ok(CleanupDecision::Cleaned)
                } else {
                    Ok(CleanupDecision::KeptForReview)
                }
            }
        }
    }
}

/// Create an isolated git worktree for a phase run.
pub fn prepare_phase_worktree(project_root: &Path, phase: &str) -> Result<PhaseWorktree> {
    let repo_root = resolve_repo_root(project_root)?;
    let base_branch = current_branch(&repo_root)?;
    let start_commit = current_commit(&repo_root, "HEAD")?;
    let worktrees_root = repo_root.join(".batty").join("worktrees");

    std::fs::create_dir_all(&worktrees_root).with_context(|| {
        format!(
            "failed to create worktrees directory {}",
            worktrees_root.display()
        )
    })?;

    let phase_slug = sanitize_phase_for_branch(phase);
    let prefix = format!("{phase_slug}-run-");
    let mut run_number = next_run_number(&repo_root, &worktrees_root, &prefix)?;

    loop {
        let branch = format!("{prefix}{run_number:03}");
        let path = worktrees_root.join(&branch);

        if path.exists() || branch_exists(&repo_root, &branch)? {
            run_number += 1;
            continue;
        }

        let path_s = path.to_string_lossy().to_string();
        let add_output = run_git(
            &repo_root,
            [
                "worktree",
                "add",
                "-b",
                branch.as_str(),
                path_s.as_str(),
                base_branch.as_str(),
            ],
        )?;
        if !add_output.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&add_output.stderr).trim()
            );
        }

        return Ok(PhaseWorktree {
            repo_root,
            base_branch,
            start_commit,
            branch,
            path,
        });
    }
}

/// Resolve the phase worktree for a run.
///
/// Behavior:
/// - If `force_new` is false, resume the latest existing `<phase>-run-###` worktree if found.
/// - Otherwise (or if none exists), create a new worktree.
///
/// Returns `(worktree, resumed_existing)`.
pub fn resolve_phase_worktree(
    project_root: &Path,
    phase: &str,
    force_new: bool,
) -> Result<(PhaseWorktree, bool)> {
    if !force_new && let Some(existing) = latest_phase_worktree(project_root, phase)? {
        return Ok((existing, true));
    }

    Ok((prepare_phase_worktree(project_root, phase)?, false))
}

/// Prepare (or reuse) one worktree per parallel agent slot for a phase.
///
/// Layout:
/// - path: `.batty/worktrees/<phase>/<agent>/`
/// - branch: `batty/<phase-slug>/<agent-slug>`
pub fn prepare_agent_worktrees(
    project_root: &Path,
    phase: &str,
    agent_names: &[String],
    force_new: bool,
) -> Result<Vec<AgentWorktree>> {
    if agent_names.is_empty() {
        bail!("parallel agent worktree preparation requires at least one agent");
    }

    let repo_root = resolve_repo_root(project_root)?;
    let base_branch = current_branch(&repo_root)?;
    let phase_slug = sanitize_phase_for_branch(phase);
    let phase_dir = repo_root.join(".batty").join("worktrees").join(phase);
    std::fs::create_dir_all(&phase_dir).with_context(|| {
        format!(
            "failed to create agent worktree phase directory {}",
            phase_dir.display()
        )
    })?;

    let mut seen_agent_slugs = HashSet::new();
    for agent in agent_names {
        let slug = sanitize_phase_for_branch(agent);
        if !seen_agent_slugs.insert(slug.clone()) {
            bail!(
                "agent names contain duplicate sanitized slug '{}'; use unique agent names",
                slug
            );
        }
    }

    let mut worktrees = Vec::with_capacity(agent_names.len());
    for agent in agent_names {
        let agent_slug = sanitize_phase_for_branch(agent);
        let branch = format!("batty/{phase_slug}/{agent_slug}");
        let path = phase_dir.join(&agent_slug);

        if force_new {
            let _ = remove_worktree(&repo_root, &path);
            let _ = delete_branch(&repo_root, &branch);
        }

        if path.exists() {
            if !branch_exists(&repo_root, &branch)? {
                bail!(
                    "agent worktree path exists but branch is missing: {} ({})",
                    path.display(),
                    branch
                );
            }
            if !worktree_registered(&repo_root, &path)? {
                bail!(
                    "agent worktree path exists but is not registered in git worktree list: {}",
                    path.display()
                );
            }
        } else {
            let path_s = path.to_string_lossy().to_string();
            let add_output = if branch_exists(&repo_root, &branch)? {
                run_git(
                    &repo_root,
                    ["worktree", "add", path_s.as_str(), branch.as_str()],
                )?
            } else {
                run_git(
                    &repo_root,
                    [
                        "worktree",
                        "add",
                        "-b",
                        branch.as_str(),
                        path_s.as_str(),
                        base_branch.as_str(),
                    ],
                )?
            };
            if !add_output.status.success() {
                bail!(
                    "git worktree add failed for agent '{}': {}",
                    agent,
                    String::from_utf8_lossy(&add_output.stderr).trim()
                );
            }
        }

        worktrees.push(AgentWorktree { branch, path });
    }

    Ok(worktrees)
}

pub fn prepare_integration_worktree(
    project_root: &Path,
    prefix: &str,
    start_ref: &str,
) -> Result<IntegrationWorktree> {
    let repo_root = resolve_repo_root(project_root)?;
    let scratch_root = repo_root.join(".batty").join("integration-worktrees");
    std::fs::create_dir_all(&scratch_root).with_context(|| {
        format!(
            "failed to create integration worktree directory {}",
            scratch_root.display()
        )
    })?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let pid = std::process::id();
    let path = scratch_root.join(format!("{prefix}{pid}-{stamp}"));
    let path_s = path.to_string_lossy().into_owned();
    let add = run_git(
        &repo_root,
        ["worktree", "add", "--detach", path_s.as_str(), start_ref],
    )?;
    if !add.status.success() {
        bail!(
            "failed to add integration worktree at {}: {}",
            path.display(),
            String::from_utf8_lossy(&add.stderr).trim()
        );
    }

    Ok(IntegrationWorktree { repo_root, path })
}

fn latest_phase_worktree(project_root: &Path, phase: &str) -> Result<Option<PhaseWorktree>> {
    let repo_root = resolve_repo_root(project_root)?;
    let base_branch = current_branch(&repo_root)?;
    let worktrees_root = repo_root.join(".batty").join("worktrees");
    if !worktrees_root.is_dir() {
        return Ok(None);
    }

    let phase_slug = sanitize_phase_for_branch(phase);
    let prefix = format!("{phase_slug}-run-");
    let mut best: Option<(u32, String, PathBuf)> = None;

    for entry in std::fs::read_dir(&worktrees_root)
        .with_context(|| format!("failed to read {}", worktrees_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let branch = entry.file_name().to_string_lossy().to_string();
        let Some(run) = parse_run_number(&branch, &prefix) else {
            continue;
        };

        if !branch_exists(&repo_root, &branch)? {
            warn!(
                branch = %branch,
                path = %path.display(),
                "skipping stale phase worktree directory without branch"
            );
            continue;
        }

        match &best {
            Some((best_run, _, _)) if run <= *best_run => {}
            _ => best = Some((run, branch, path)),
        }
    }

    let Some((_, branch, path)) = best else {
        return Ok(None);
    };

    let start_commit = current_commit(&repo_root, &branch)?;
    Ok(Some(PhaseWorktree {
        repo_root,
        base_branch,
        start_commit,
        branch,
        path,
    }))
}

fn resolve_repo_root(project_root: &Path) -> Result<PathBuf> {
    let output = crate::git::cmd(project_root)
        .args(["rev-parse", "--show-toplevel"])
        .allow_fail()
        .run()
        .with_context(|| {
            format!(
                "failed while trying to resolve the repository root: could not execute `git rev-parse --show-toplevel` in {}",
                project_root.display()
            )
        })?;
    if !output.status.success() {
        bail!(
            "not a git repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        bail!("git rev-parse returned empty repository root");
    }
    Ok(PathBuf::from(root))
}

fn current_branch(repo_root: &Path) -> Result<String> {
    let output = run_git(repo_root, ["branch", "--show-current"])?;
    if !output.status.success() {
        bail!(
            "failed to determine current branch: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        bail!("detached HEAD is not supported for phase worktree runs; checkout a branch first");
    }
    Ok(branch)
}

fn next_run_number(repo_root: &Path, worktrees_root: &Path, prefix: &str) -> Result<u32> {
    let mut max_run = 0;

    let refs = run_git(
        repo_root,
        ["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )?;
    if !refs.status.success() {
        bail!(
            "failed to list branches: {}",
            String::from_utf8_lossy(&refs.stderr).trim()
        );
    }

    for branch in String::from_utf8_lossy(&refs.stdout).lines() {
        if let Some(run) = parse_run_number(branch, prefix) {
            max_run = max_run.max(run);
        }
    }

    if worktrees_root.is_dir() {
        for entry in std::fs::read_dir(worktrees_root)
            .with_context(|| format!("failed to read {}", worktrees_root.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(run) = parse_run_number(name.as_ref(), prefix) {
                max_run = max_run.max(run);
            }
        }
    }

    Ok(max_run + 1)
}

fn parse_run_number(name: &str, prefix: &str) -> Option<u32> {
    let suffix = name.strip_prefix(prefix)?;
    if suffix.len() < 3 || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok()
}

fn sanitize_phase_for_branch(phase: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;

    for c in phase.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "phase".to_string()
    } else {
        slug
    }
}

/// Run git in `repo_root` and return its output whatever the exit status;
/// callers inspect `status`. Spawn failures and timeouts are errors.
fn run_git<I, S>(repo_root: &Path, args: I) -> Result<crate::git::GitOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let command = crate::git::cmd(repo_root).args(args).allow_fail();
    let rendered = command.display();
    command
        .run()
        .with_context(|| format!("failed to execute `{rendered}`"))
}

fn branch_exists(repo_root: &Path, branch: &str) -> Result<bool> {
    let ref_name = format!("refs/heads/{branch}");
    let output = run_git(
        repo_root,
        ["show-ref", "--verify", "--quiet", ref_name.as_str()],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to check branch '{}': {}",
            branch,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn worktree_registered(repo_root: &Path, path: &Path) -> Result<bool> {
    let output = run_git(repo_root, ["worktree", "list", "--porcelain"])?;
    if !output.status.success() {
        bail!(
            "failed to list worktrees: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    // git prints canonical paths (e.g. /private/var/... on macOS) while the
    // caller may hold a symlinked spelling (/var/...); compare canonical forms.
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let target = canonical(path);
    let listed = String::from_utf8_lossy(&output.stdout);
    for line in listed.lines() {
        if let Some(candidate) = line.strip_prefix("worktree ")
            && canonical(Path::new(candidate.trim())) == target
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_merged_into_base(repo_root: &Path, branch: &str, base_branch: &str) -> Result<bool> {
    let output = run_git(
        repo_root,
        ["merge-base", "--is-ancestor", branch, base_branch],
    )?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to check merge status for '{}' into '{}': {}",
            branch,
            base_branch,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn current_commit(repo_root: &Path, rev: &str) -> Result<String> {
    let output = run_git(repo_root, ["rev-parse", rev])?;
    if !output.status.success() {
        bail!(
            "failed to resolve revision '{}': {}",
            rev,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.is_empty() {
        bail!("git rev-parse returned empty commit for '{rev}'");
    }
    Ok(commit)
}

fn remove_worktree(repo_root: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let path_s = path.to_string_lossy().to_string();
    let output = run_git(
        repo_root,
        ["worktree", "remove", "--force", path_s.as_str()],
    )?;
    if !output.status.success() {
        bail!(
            "failed to remove worktree '{}': {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn delete_branch(repo_root: &Path, branch: &str) -> Result<()> {
    if !branch_exists(repo_root, branch)? {
        return Ok(());
    }

    let output = run_git(repo_root, ["branch", "-D", branch])?;
    if !output.status.success() {
        bail!(
            "failed to delete branch '{}': {}",
            branch,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Sync the phase board from the source tree into the worktree.
///
/// Worktrees are created from committed state, so any uncommitted kanban
/// changes (new tasks, reworked boards, etc.) would be lost. This copies
/// the phase directory from `source_kanban_root/<phase>/` into
/// `worktree_kanban_root/<phase>/`, overwriting whatever git checked out.
///
/// Only syncs when the source directory exists and differs from the
/// worktree (i.e., the source tree has uncommitted kanban changes).
pub fn sync_phase_board_to_worktree(
    project_root: &Path,
    worktree_root: &Path,
    phase: &str,
) -> Result<()> {
    let source_phase_dir = crate::paths::resolve_kanban_root(project_root).join(phase);
    if !source_phase_dir.is_dir() {
        return Ok(());
    }

    let dest_kanban_root = crate::paths::resolve_kanban_root(worktree_root);
    let dest_phase_dir = dest_kanban_root.join(phase);

    // Remove stale destination and copy fresh.
    if dest_phase_dir.exists() {
        std::fs::remove_dir_all(&dest_phase_dir).with_context(|| {
            format!(
                "failed to remove stale phase board at {}",
                dest_phase_dir.display()
            )
        })?;
    }

    copy_dir_recursive(&source_phase_dir, &dest_phase_dir).with_context(|| {
        format!(
            "failed to sync phase board from {} to {}",
            source_phase_dir.display(),
            dest_phase_dir.display()
        )
    })?;

    info!(
        phase = phase,
        source = %source_phase_dir.display(),
        dest = %dest_phase_dir.display(),
        "synced phase board into worktree"
    );
    Ok(())
}

/// Check if all commits on `branch` since diverging from `base` are already
/// present on `base` (e.g., via cherry-pick).
///
/// Uses `git cherry <base> <branch>` — lines starting with `-` are already on
/// base. If ALL lines start with `-` (or output is empty), the branch is fully
/// merged.
pub fn branch_fully_merged(repo_root: &Path, branch: &str, base: &str) -> Result<bool> {
    let output = run_git(repo_root, ["cherry", base, branch])?;
    if !output.status.success() {
        bail!(
            "git cherry failed for '{}' against '{}': {}",
            branch,
            base,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Lines starting with '+' are commits NOT on base.
        if trimmed.starts_with('+') {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Count commits on the current branch that are ahead of `base` (e.g. "main").
/// Returns 0 if the branch is at or behind base.
pub fn commits_ahead(worktree_path: &Path, base: &str) -> Result<usize> {
    Ok(ops::ahead_count(worktree_path, base, "HEAD")? as usize)
}

/// Check if a worktree has uncommitted changes (staged, unstaged or
/// untracked), counting every path including Batty runtime dirs.
pub fn has_uncommitted_changes(worktree_path: &Path) -> Result<bool> {
    ops::is_dirty(worktree_path, ops::ExcludePolicy::Nothing)
}

/// Get the current branch name for a repository/worktree path.
pub fn git_current_branch(path: &Path) -> Result<String> {
    let output = run_git(path, ["branch", "--show-current"])?;
    if !output.status.success() {
        bail!(
            "failed to determine current branch in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        bail!(
            "detached HEAD in {}; cannot determine branch",
            path.display()
        );
    }
    Ok(branch)
}

fn merge_base_is_ancestor(path: &Path, older: &str, newer: &str) -> Result<bool> {
    let output = run_git(path, ["merge-base", "--is-ancestor", older, newer])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to compare {} and {} in {}: {}",
            older,
            newer,
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn preferred_trunk_start_ref(path: &Path, trunk_branch: &str) -> Result<MainStartRefSelection> {
    let local_ref = format!("refs/heads/{trunk_branch}");
    let remote_ref = format!("refs/remotes/origin/{trunk_branch}");
    let local_branch = trunk_branch.to_string();
    let remote_branch = format!("origin/{trunk_branch}");

    let has_local = ref_exists(path, &local_ref)?;
    let has_remote = ref_exists(path, &remote_ref)?;
    if !has_remote {
        return Ok(MainStartRefSelection {
            ref_name: local_branch,
            fallback_reason: Some("stale_origin_fallback ahead=0 origin_unreachable".to_string()),
        });
    }
    if !has_local {
        return Ok(MainStartRefSelection {
            ref_name: remote_branch,
            fallback_reason: None,
        });
    }

    let local_main = current_commit(path, &local_ref)?;
    let remote_main = current_commit(path, &remote_ref)?;
    if local_main == remote_main {
        return Ok(MainStartRefSelection {
            ref_name: remote_branch,
            fallback_reason: None,
        });
    }

    if merge_base_is_ancestor(path, &remote_ref, &local_ref)? {
        let ahead = rev_count(path, &format!("origin/{trunk_branch}..{trunk_branch}"))?;
        return Ok(MainStartRefSelection {
            ref_name: local_branch,
            fallback_reason: Some(format!("stale_origin_fallback ahead={ahead}")),
        });
    }

    if merge_base_is_ancestor(path, &local_ref, &remote_ref)? {
        return Ok(MainStartRefSelection {
            ref_name: remote_branch,
            fallback_reason: None,
        });
    }

    let ahead = rev_count(path, &format!("origin/{trunk_branch}..{trunk_branch}"))?;
    let origin_ahead = rev_count(path, &format!("{trunk_branch}..origin/{trunk_branch}"))?;
    Ok(MainStartRefSelection {
        ref_name: local_branch,
        fallback_reason: Some(format!(
            "stale_origin_fallback ahead={ahead} divergent origin_ahead={origin_ahead}"
        )),
    })
}

#[cfg(test)]
fn preferred_main_start_ref(path: &Path) -> Result<MainStartRefSelection> {
    preferred_trunk_start_ref(path, "main")
}

pub fn ensure_baseline_branch_from_trunk(
    repo_path: &Path,
    base_branch: &str,
    trunk_branch: &str,
) -> Result<Option<BaselineBranchRepair>> {
    let base_ref = format!("refs/heads/{base_branch}");
    if ref_exists(repo_path, &base_ref)? {
        return Ok(None);
    }

    let selection = preferred_trunk_start_ref(repo_path, trunk_branch)
        .with_context(|| {
            format!(
                "failed to select configured trunk '{trunk_branch}' while recreating missing baseline branch '{base_branch}' in {}",
                repo_path.display()
            )
        })?;
    let start_commit = current_commit(repo_path, selection.ref_name.as_str())
        .with_context(|| {
            format!(
                "failed to resolve configured trunk ref '{}' for missing baseline branch '{}' in {}",
                selection.ref_name,
                base_branch,
                repo_path.display()
            )
        })?;

    let update = run_git(
        repo_path,
        ["update-ref", base_ref.as_str(), start_commit.as_str()],
    )?;
    if !update.status.success() {
        bail!(
            "failed to recreate missing baseline branch '{}' in {} from configured trunk ref '{}' (trunk_branch='{}', commit={}): {}",
            base_branch,
            repo_path.display(),
            selection.ref_name,
            trunk_branch,
            start_commit,
            String::from_utf8_lossy(&update.stderr).trim()
        );
    }

    Ok(Some(BaselineBranchRepair {
        branch: base_branch.to_string(),
        start_ref: selection.ref_name,
        start_commit,
        fallback_reason: selection.fallback_reason,
    }))
}

#[cfg(test)]
pub fn ensure_worktree_branch_for_dispatch(
    worktree_path: &Path,
    expected_branch: &str,
) -> Result<DispatchBranchReset> {
    ensure_worktree_branch_for_dispatch_from_trunk(worktree_path, expected_branch, "main")
}

pub fn ensure_worktree_branch_for_dispatch_from_trunk(
    worktree_path: &Path,
    expected_branch: &str,
    trunk_branch: &str,
) -> Result<DispatchBranchReset> {
    let current_branch = git_current_branch(worktree_path)?;
    if current_branch == expected_branch {
        return Ok(DispatchBranchReset {
            changed: false,
            start_ref: current_branch,
            fallback_reason: None,
            reset_reason: None,
        });
    }

    let selection = preferred_trunk_start_ref(worktree_path, trunk_branch)?;
    let commit_message = format!("wip: auto-save before worktree reset [{current_branch}]");
    let reason = prepare_worktree_for_reset(
        worktree_path,
        &commit_message,
        Duration::from_secs(5),
        PreserveFailureMode::SkipReset,
        "dispatch/branch-reset",
    )?;
    info!(
        worktree = %worktree_path.display(),
        expected_branch,
        reset_reason = reason.as_str(),
        "prepared worktree for dispatch branch reset"
    );
    if !reason.reset_performed() {
        return Ok(DispatchBranchReset {
            changed: false,
            start_ref: selection.ref_name,
            fallback_reason: selection.fallback_reason,
            reset_reason: Some(reason),
        });
    }

    // #659: archive commits ahead of the start ref on `expected_branch` before
    // the destructive `checkout -B expected_branch <start_ref>` below.
    // A failed archive must abort the reset: rewriting the branch without a
    // saved copy would drop its unmerged commits.
    archive_branch_if_commits_ahead(
        worktree_path,
        expected_branch,
        selection.ref_name.as_str(),
        "dispatch/branch-reset",
    )
    .with_context(|| {
        format!(
            "aborting branch reset of '{}' in {}: could not archive its commits",
            expected_branch,
            worktree_path.display()
        )
    })?;
    ops::log_mutation_audit(
        worktree_path,
        "dispatch/branch-reset",
        "git checkout -B",
        &ops::dirty_paths(worktree_path, ops::ExcludePolicy::BattyRuntime).unwrap_or_default(),
    );
    let checkout = run_git(
        worktree_path,
        [
            "checkout",
            "-B",
            expected_branch,
            selection.ref_name.as_str(),
        ],
    )?;
    if !checkout.status.success() {
        bail!(
            "failed to checkout '{}' from '{}' in {}: {}",
            expected_branch,
            selection.ref_name,
            worktree_path.display(),
            String::from_utf8_lossy(&checkout.stderr).trim()
        );
    }

    Ok(DispatchBranchReset {
        changed: true,
        start_ref: selection.ref_name,
        fallback_reason: selection.fallback_reason,
        reset_reason: Some(reason),
    })
}

/// Reset a worktree to point at its base branch. Used to clean up after a
/// cherry-pick merge has made the task branch redundant.
#[cfg(test)]
pub fn reset_worktree_to_base(worktree_path: &Path, base_branch: &str) -> Result<()> {
    let branch = git_current_branch(worktree_path).unwrap_or_else(|_| base_branch.to_string());
    let commit_message = format!("wip: auto-save before worktree reset [{branch}]");
    reset_worktree_to_base_with_options_for(
        worktree_path,
        base_branch,
        &commit_message,
        Duration::from_secs(5),
        PreserveFailureMode::SkipReset,
        "worktree/reset",
    )?;
    Ok(())
}

#[cfg(test)]
pub fn reset_worktree_to_base_with_options(
    worktree_path: &Path,
    base_branch: &str,
    commit_message: &str,
    timeout: Duration,
    preserve_failure_mode: PreserveFailureMode,
) -> Result<WorktreeResetReason> {
    reset_worktree_to_base_with_options_for(
        worktree_path,
        base_branch,
        commit_message,
        timeout,
        preserve_failure_mode,
        "worktree/reset",
    )
}

#[cfg(test)]
pub fn reset_worktree_to_base_with_options_for(
    worktree_path: &Path,
    base_branch: &str,
    commit_message: &str,
    timeout: Duration,
    preserve_failure_mode: PreserveFailureMode,
    subsystem: &str,
) -> Result<WorktreeResetReason> {
    reset_worktree_to_base_with_options_for_trunk(
        worktree_path,
        base_branch,
        commit_message,
        timeout,
        preserve_failure_mode,
        subsystem,
        "main",
    )
}

pub fn reset_worktree_to_base_with_options_for_trunk(
    worktree_path: &Path,
    base_branch: &str,
    commit_message: &str,
    timeout: Duration,
    preserve_failure_mode: PreserveFailureMode,
    subsystem: &str,
    trunk_branch: &str,
) -> Result<WorktreeResetReason> {
    ops::reset_to_base(
        worktree_path,
        base_branch,
        trunk_branch,
        &ops::ResetPolicy {
            subsystem,
            dirty: ops::DirtyPolicy::Preserve {
                commit_message,
                timeout,
                on_failure: preserve_failure_mode,
            },
            archive_current_branch: false,
        },
    )
}

#[cfg(test)]
pub fn reset_worktree_to_base_if_clean(
    worktree_path: &Path,
    base_branch: &str,
    subsystem: &str,
) -> Result<WorktreeResetReason> {
    reset_worktree_to_base_if_clean_from_trunk(worktree_path, base_branch, subsystem, "main")
}

/// Background reset that never touches a dirty lane: returns
/// `PreserveFailedResetSkipped` when user changes are present.
pub fn reset_worktree_to_base_if_clean_from_trunk(
    worktree_path: &Path,
    base_branch: &str,
    subsystem: &str,
    trunk_branch: &str,
) -> Result<WorktreeResetReason> {
    ops::reset_to_base(
        worktree_path,
        base_branch,
        trunk_branch,
        &ops::ResetPolicy {
            subsystem,
            dirty: ops::DirtyPolicy::Refuse,
            archive_current_branch: false,
        },
    )
}

/// Converging recovery for background health checks (merge-conflict and
/// stale-branch recovery). Unlike [`reset_worktree_to_base_if_clean_from_trunk`],
/// which refuses dirty lanes forever, this first aborts any in-progress
/// merge/rebase/cherry-pick (whose conflicted paths are what make the lane
/// "dirty"), then auto-saves remaining dirty work as a WIP commit on the
/// current branch and archives that branch's commits before resetting to
/// `base_branch`. When the dirty work cannot be preserved the reset is
/// skipped (`PreserveFailedResetSkipped`) so nothing is lost.
pub(crate) fn recover_worktree_to_base_preserving(
    worktree_path: &Path,
    base_branch: &str,
    subsystem: &str,
    trunk_branch: &str,
) -> Result<WorktreeResetReason> {
    abort_in_progress_git_operations(worktree_path);
    let current_branch = git_current_branch(worktree_path).ok();
    let commit_message = format!(
        "wip: auto-save before {subsystem} [{}]",
        current_branch.as_deref().unwrap_or("detached")
    );
    ops::reset_to_base(
        worktree_path,
        base_branch,
        trunk_branch,
        &ops::ResetPolicy {
            subsystem,
            dirty: ops::DirtyPolicy::Preserve {
                commit_message: &commit_message,
                timeout: Duration::from_secs(5),
                on_failure: PreserveFailureMode::SkipReset,
            },
            archive_current_branch: true,
        },
    )
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::git_ok as git;
    use std::fs;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    fn init_repo() -> Option<tempfile::TempDir> {
        if !git_available() {
            return None;
        }

        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(
            tmp.path(),
            &["config", "user.email", "batty-test@example.com"],
        );
        git(tmp.path(), &["config", "user.name", "Batty Test"]);

        fs::write(tmp.path().join("README.md"), "init\n").unwrap();
        git(tmp.path(), &["add", "README.md"]);
        git(tmp.path(), &["commit", "-q", "-m", "init"]);

        Some(tmp)
    }

    fn cleanup_worktree(repo_root: &Path, worktree: &PhaseWorktree) {
        let _ = remove_worktree(repo_root, &worktree.path);
        let _ = delete_branch(repo_root, &worktree.branch);
    }

    fn cleanup_agent_worktrees(repo_root: &Path, worktrees: &[AgentWorktree]) {
        for wt in worktrees {
            let _ = remove_worktree(repo_root, &wt.path);
            let _ = delete_branch(repo_root, &wt.branch);
        }
    }

    #[test]
    fn sanitize_phase_for_branch_normalizes_phase() {
        assert_eq!(sanitize_phase_for_branch("phase-2.5"), "phase-2-5");
        assert_eq!(sanitize_phase_for_branch("Phase 7"), "phase-7");
        assert_eq!(sanitize_phase_for_branch("///"), "phase");
    }

    #[test]
    fn parse_run_number_extracts_suffix() {
        assert_eq!(parse_run_number("phase-2-run-001", "phase-2-run-"), Some(1));
        assert_eq!(
            parse_run_number("phase-2-run-1234", "phase-2-run-"),
            Some(1234)
        );
        assert_eq!(parse_run_number("phase-2-run-aa1", "phase-2-run-"), None);
        assert_eq!(parse_run_number("other-001", "phase-2-run-"), None);
    }

    #[test]
    fn prepare_phase_worktree_increments_run_number() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let first = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let second = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();

        assert!(
            first.branch.ends_with("001"),
            "first branch: {}",
            first.branch
        );
        assert!(
            second.branch.ends_with("002"),
            "second branch: {}",
            second.branch
        );
        assert!(first.path.is_dir());
        assert!(second.path.is_dir());

        cleanup_worktree(tmp.path(), &first);
        cleanup_worktree(tmp.path(), &second);
    }

    #[test]
    fn finalize_keeps_unmerged_completed_worktree() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let decision = worktree.finalize(RunOutcome::Completed).unwrap();

        assert_eq!(decision, CleanupDecision::KeptForReview);
        assert!(worktree.path.exists());
        assert!(branch_exists(tmp.path(), &worktree.branch).unwrap());

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn finalize_keeps_failed_worktree() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let decision = worktree.finalize(RunOutcome::Failed).unwrap();

        assert_eq!(decision, CleanupDecision::KeptForFailure);
        assert!(worktree.path.exists());
        assert!(branch_exists(tmp.path(), &worktree.branch).unwrap());

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn finalize_cleans_when_merged() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();

        fs::write(worktree.path.join("work.txt"), "done\n").unwrap();
        git(&worktree.path, &["add", "work.txt"]);
        git(&worktree.path, &["commit", "-q", "-m", "worktree change"]);

        git(
            tmp.path(),
            &["merge", "--no-ff", "--no-edit", worktree.branch.as_str()],
        );

        let decision = worktree.finalize(RunOutcome::Completed).unwrap();
        assert_eq!(decision, CleanupDecision::Cleaned);
        assert!(!worktree.path.exists());
        assert!(!branch_exists(tmp.path(), &worktree.branch).unwrap());
    }

    #[test]
    fn resolve_phase_worktree_resumes_latest_existing_by_default() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let first = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let second = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();

        let (resolved, resumed) = resolve_phase_worktree(tmp.path(), "phase-2.5", false).unwrap();
        assert!(
            resumed,
            "expected default behavior to resume existing worktree"
        );
        assert_eq!(
            resolved.branch, second.branch,
            "should resume latest run branch"
        );
        assert_eq!(resolved.path, second.path, "should resume latest run path");

        cleanup_worktree(tmp.path(), &first);
        cleanup_worktree(tmp.path(), &second);
    }

    #[test]
    fn resolve_phase_worktree_force_new_creates_next_run() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let first = prepare_phase_worktree(tmp.path(), "phase-2.5").unwrap();
        let (resolved, resumed) = resolve_phase_worktree(tmp.path(), "phase-2.5", true).unwrap();

        assert!(!resumed, "force-new should never resume prior worktree");
        assert_ne!(resolved.branch, first.branch);
        assert!(
            resolved.branch.ends_with("002"),
            "branch: {}",
            resolved.branch
        );

        cleanup_worktree(tmp.path(), &first);
        cleanup_worktree(tmp.path(), &resolved);
    }

    #[test]
    fn resolve_phase_worktree_without_existing_creates_new() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let (resolved, resumed) = resolve_phase_worktree(tmp.path(), "phase-2.5", false).unwrap();
        assert!(!resumed);
        assert!(
            resolved.branch.ends_with("001"),
            "branch: {}",
            resolved.branch
        );

        cleanup_worktree(tmp.path(), &resolved);
    }

    #[test]
    fn prepare_agent_worktrees_creates_layout_and_branches() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent-1".to_string(), "agent-2".to_string()];
        let worktrees = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();

        assert_eq!(worktrees.len(), 2);
        // Compare canonicalized paths to handle macOS /var vs /private/var symlink
        assert_eq!(
            worktrees[0].path.canonicalize().unwrap(),
            tmp.path()
                .join(".batty")
                .join("worktrees")
                .join("phase-4")
                .join("agent-1")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(worktrees[0].branch, "batty/phase-4/agent-1");
        assert!(branch_exists(tmp.path(), "batty/phase-4/agent-1").unwrap());
        assert!(branch_exists(tmp.path(), "batty/phase-4/agent-2").unwrap());

        cleanup_agent_worktrees(tmp.path(), &worktrees);
    }

    #[test]
    fn prepare_agent_worktrees_reuses_existing_agent_paths() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent-1".to_string(), "agent-2".to_string()];
        let first = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();
        let second = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();

        assert_eq!(first[0].path, second[0].path);
        assert_eq!(first[1].path, second[1].path);
        assert_eq!(first[0].branch, second[0].branch);
        assert_eq!(first[1].branch, second[1].branch);

        cleanup_agent_worktrees(tmp.path(), &first);
    }

    #[test]
    fn prepare_agent_worktrees_rejects_duplicate_sanitized_names() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent 1".to_string(), "agent-1".to_string()];
        let err = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("duplicate sanitized slug"));
    }

    #[test]
    fn prepare_agent_worktrees_force_new_recreates_worktrees() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let names = vec!["agent-1".to_string()];
        let first = prepare_agent_worktrees(tmp.path(), "phase-4", &names, false).unwrap();

        fs::write(first[0].path.join("agent.txt"), "agent-1\n").unwrap();
        git(&first[0].path, &["add", "agent.txt"]);
        git(&first[0].path, &["commit", "-q", "-m", "agent work"]);

        let second = prepare_agent_worktrees(tmp.path(), "phase-4", &names, true).unwrap();
        let listing = run_git(tmp.path(), ["branch", "--list", "batty/phase-4/agent-1"]).unwrap();
        assert!(listing.status.success());
        assert!(second[0].path.exists());

        cleanup_agent_worktrees(tmp.path(), &second);
    }

    #[test]
    fn sync_phase_board_copies_uncommitted_tasks_into_worktree() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a committed phase board with one task.
        let kanban = tmp.path().join(".batty").join("kanban");
        let phase_dir = kanban.join("my-phase").join("tasks");
        fs::create_dir_all(&phase_dir).unwrap();
        fs::write(phase_dir.join("001-old.md"), "old task\n").unwrap();
        fs::write(
            kanban.join("my-phase").join("config.yml"),
            "version: 10\nnext_id: 2\n",
        )
        .unwrap();
        git(tmp.path(), &["add", ".batty"]);
        git(tmp.path(), &["commit", "-q", "-m", "add phase board"]);

        // Create a worktree — it will have the committed (old) board.
        let worktree = prepare_phase_worktree(tmp.path(), "my-phase").unwrap();
        let wt_task = worktree
            .path
            .join(".batty")
            .join("kanban")
            .join("my-phase")
            .join("tasks")
            .join("001-old.md");
        assert!(wt_task.exists(), "worktree should have committed task");

        // Now add a new task to the source tree (uncommitted).
        fs::write(phase_dir.join("002-new.md"), "new task\n").unwrap();

        // Sync.
        sync_phase_board_to_worktree(tmp.path(), &worktree.path, "my-phase").unwrap();

        // Worktree should now have both tasks.
        let wt_tasks_dir = worktree
            .path
            .join(".batty")
            .join("kanban")
            .join("my-phase")
            .join("tasks");
        assert!(wt_tasks_dir.join("001-old.md").exists());
        assert!(
            wt_tasks_dir.join("002-new.md").exists(),
            "uncommitted task should be synced into worktree"
        );

        // The new file content should match.
        let content = fs::read_to_string(wt_tasks_dir.join("002-new.md")).unwrap();
        assert_eq!(content, "new task\n");

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn sync_phase_board_overwrites_stale_worktree_board() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a committed phase board.
        let kanban = tmp.path().join(".batty").join("kanban");
        let phase_dir = kanban.join("my-phase").join("tasks");
        fs::create_dir_all(&phase_dir).unwrap();
        fs::write(phase_dir.join("001-old.md"), "original\n").unwrap();
        fs::write(
            kanban.join("my-phase").join("config.yml"),
            "version: 10\nnext_id: 2\n",
        )
        .unwrap();
        git(tmp.path(), &["add", ".batty"]);
        git(tmp.path(), &["commit", "-q", "-m", "add phase board"]);

        let worktree = prepare_phase_worktree(tmp.path(), "my-phase").unwrap();

        // Rewrite the source task (uncommitted change).
        fs::write(phase_dir.join("001-old.md"), "rewritten\n").unwrap();

        sync_phase_board_to_worktree(tmp.path(), &worktree.path, "my-phase").unwrap();

        let wt_content = fs::read_to_string(
            worktree
                .path
                .join(".batty")
                .join("kanban")
                .join("my-phase")
                .join("tasks")
                .join("001-old.md"),
        )
        .unwrap();
        assert_eq!(
            wt_content, "rewritten\n",
            "worktree board should reflect source tree changes"
        );

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn sync_phase_board_noop_when_source_missing() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let worktree = prepare_phase_worktree(tmp.path(), "nonexistent").unwrap();

        // Should not error when source phase dir doesn't exist.
        sync_phase_board_to_worktree(tmp.path(), &worktree.path, "nonexistent").unwrap();

        cleanup_worktree(tmp.path(), &worktree);
    }

    #[test]
    fn branch_fully_merged_true_after_cherry_pick() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a feature branch with a commit.
        git(tmp.path(), &["checkout", "-b", "feature"]);
        fs::write(tmp.path().join("feature.txt"), "feature work\n").unwrap();
        git(tmp.path(), &["add", "feature.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "add feature"]);

        // Go back to main and cherry-pick the commit.
        git(tmp.path(), &["checkout", "main"]);
        git(tmp.path(), &["cherry-pick", "feature"]);

        // Now all commits on feature are present on main.
        assert!(branch_fully_merged(tmp.path(), "feature", "main").unwrap());
    }

    #[test]
    fn branch_fully_merged_false_with_unique_commits() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a feature branch with a commit NOT on main.
        git(tmp.path(), &["checkout", "-b", "feature"]);
        fs::write(tmp.path().join("unique.txt"), "unique work\n").unwrap();
        git(tmp.path(), &["add", "unique.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "unique commit"]);
        git(tmp.path(), &["checkout", "main"]);

        assert!(!branch_fully_merged(tmp.path(), "feature", "main").unwrap());
    }

    #[test]
    fn branch_fully_merged_true_when_same_tip() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Feature branch at the same commit as main — no unique commits.
        git(tmp.path(), &["checkout", "-b", "feature"]);
        git(tmp.path(), &["checkout", "main"]);

        assert!(branch_fully_merged(tmp.path(), "feature", "main").unwrap());
    }

    #[test]
    fn branch_fully_merged_false_partial_merge() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a feature branch with two commits.
        git(tmp.path(), &["checkout", "-b", "feature"]);
        fs::write(tmp.path().join("a.txt"), "a\n").unwrap();
        git(tmp.path(), &["add", "a.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "first"]);

        fs::write(tmp.path().join("b.txt"), "b\n").unwrap();
        git(tmp.path(), &["add", "b.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "second"]);

        // Cherry-pick only the first commit onto main.
        git(tmp.path(), &["checkout", "main"]);
        git(tmp.path(), &["cherry-pick", "feature~1"]);

        // One commit is still unique — should be false.
        assert!(!branch_fully_merged(tmp.path(), "feature", "main").unwrap());
    }

    #[test]
    fn git_current_branch_returns_branch_name() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Default branch after init_repo is "main" (or whatever git defaults to).
        let branch = git_current_branch(tmp.path()).unwrap();
        // The init_repo doesn't specify -b, so branch could be "main" or "master".
        assert!(!branch.is_empty(), "should return a non-empty branch name");
    }

    #[test]
    fn reset_worktree_to_base_switches_branch() {
        let Some(tmp) = init_repo() else {
            return;
        };

        // Create a worktree on a feature branch.
        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature-reset",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("work.txt"), "work\n").unwrap();
        git(&wt_path, &["add", "work.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "work on feature"]);

        // Verify we're on the feature branch.
        let branch_before = git_current_branch(&wt_path).unwrap();
        assert_eq!(branch_before, "feature-reset");

        // Create a base branch for the worktree to reset to.
        git(tmp.path(), &["branch", "eng-main/test-eng"]);

        let reason = reset_worktree_to_base_with_options(
            &wt_path,
            "eng-main/test-eng",
            "wip: auto-save before worktree reset [feature-reset]",
            Duration::from_secs(5),
            PreserveFailureMode::SkipReset,
        )
        .unwrap();

        let branch_after = git_current_branch(&wt_path).unwrap();
        assert_eq!(branch_after, "eng-main/test-eng");
        assert_eq!(reason, WorktreeResetReason::CleanReset);

        // Cleanup
        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "feature-reset"]);
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-main/test-eng"]);
    }

    #[test]
    fn reset_worktree_to_base_recreates_missing_branch_from_main() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "feature-reset",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("work.txt"), "work\n").unwrap();
        git(&wt_path, &["add", "work.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "work on feature"]);

        let reason = reset_worktree_to_base_with_options(
            &wt_path,
            "eng-main/test-eng",
            "wip: auto-save before worktree reset [feature-reset]",
            Duration::from_secs(5),
            PreserveFailureMode::SkipReset,
        )
        .unwrap();

        assert_eq!(reason, WorktreeResetReason::CleanReset);
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-main/test-eng");
        let head = current_commit(&wt_path, "HEAD").unwrap();
        let main = current_commit(tmp.path(), "main").unwrap();
        assert_eq!(head, main);

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "feature-reset"]);
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-main/test-eng"]);
    }

    #[test]
    fn ensure_worktree_branch_for_dispatch_keeps_matching_branch() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-1-502",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );

        let before = run_git(&wt_path, ["rev-parse", "HEAD"]).unwrap().stdout;
        let reset = ensure_worktree_branch_for_dispatch(&wt_path, "eng-1-1-502").unwrap();
        let after = run_git(&wt_path, ["rev-parse", "HEAD"]).unwrap().stdout;

        assert!(!reset.changed);
        assert!(reset.reset_reason.is_none());
        assert_eq!(before, after);

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-502"]);
    }

    #[test]
    fn ensure_worktree_branch_for_dispatch_resets_mismatched_branch() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-1-500",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("work.txt"), "old work\n").unwrap();
        git(&wt_path, &["add", "work.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "old task"]);

        let reset = ensure_worktree_branch_for_dispatch(&wt_path, "eng-1-1-502").unwrap();
        let head = run_git(&wt_path, ["rev-parse", "HEAD"]).unwrap().stdout;
        let main = run_git(tmp.path(), ["rev-parse", "main"]).unwrap().stdout;

        assert!(reset.changed);
        assert_eq!(reset.reset_reason, Some(WorktreeResetReason::CleanReset));
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-1-1-502");
        assert_eq!(head, main);

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-500"]);
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-502"]);
    }

    #[test]
    fn preferred_main_start_ref_uses_origin_when_equal() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let main = current_commit(tmp.path(), "main").unwrap();
        git(
            tmp.path(),
            &["update-ref", "refs/remotes/origin/main", main.as_str()],
        );

        let selection = preferred_main_start_ref(tmp.path()).unwrap();
        assert_eq!(selection.ref_name, "origin/main");
        assert!(selection.fallback_reason.is_none());
    }

    #[test]
    fn preferred_trunk_start_ref_uses_configured_mainline_origin_when_equal() {
        let Some(tmp) = init_repo() else {
            return;
        };
        git(tmp.path(), &["checkout", "-b", "mainline"]);
        let mainline = current_commit(tmp.path(), "mainline").unwrap();
        git(
            tmp.path(),
            &[
                "update-ref",
                "refs/remotes/origin/mainline",
                mainline.as_str(),
            ],
        );

        let selection = preferred_trunk_start_ref(tmp.path(), "mainline").unwrap();

        assert_eq!(selection.ref_name, "origin/mainline");
        assert!(selection.fallback_reason.is_none());
    }

    #[test]
    fn ensure_baseline_branch_from_trunk_recreates_missing_branch_from_mainline() {
        let Some(tmp) = init_repo() else {
            return;
        };
        git(tmp.path(), &["checkout", "-b", "mainline"]);
        fs::write(tmp.path().join("mainline.txt"), "mainline\n").unwrap();
        git(tmp.path(), &["add", "mainline.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "advance mainline"]);
        git(tmp.path(), &["branch", "-D", "main"]);

        let repair = ensure_baseline_branch_from_trunk(tmp.path(), "eng-main/test-eng", "mainline")
            .unwrap()
            .expect("missing baseline branch should be recreated");

        assert_eq!(repair.branch, "eng-main/test-eng");
        assert_eq!(repair.start_ref, "mainline");
        assert_eq!(
            current_commit(tmp.path(), "eng-main/test-eng").unwrap(),
            current_commit(tmp.path(), "mainline").unwrap()
        );
    }

    #[test]
    fn ensure_baseline_branch_from_trunk_reports_missing_trunk_details() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let error = ensure_baseline_branch_from_trunk(tmp.path(), "eng-main/test-eng", "mainline")
            .unwrap_err();
        let message = error.to_string();

        assert!(
            message.contains("configured trunk ref 'mainline'"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("eng-main/test-eng"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains(tmp.path().to_string_lossy().as_ref()),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn preferred_main_start_ref_falls_back_to_local_main_when_origin_is_behind() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let frozen = current_commit(tmp.path(), "main").unwrap();
        git(
            tmp.path(),
            &["update-ref", "refs/remotes/origin/main", frozen.as_str()],
        );
        fs::write(tmp.path().join("local.txt"), "local\n").unwrap();
        git(tmp.path(), &["add", "local.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "local advance"]);

        let selection = preferred_main_start_ref(tmp.path()).unwrap();
        assert_eq!(selection.ref_name, "main");
        assert_eq!(
            selection.fallback_reason.as_deref(),
            Some("stale_origin_fallback ahead=1")
        );
    }

    #[test]
    fn preferred_main_start_ref_falls_back_to_local_main_when_diverged() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let base = current_commit(tmp.path(), "main").unwrap();
        fs::write(tmp.path().join("local.txt"), "local\n").unwrap();
        git(tmp.path(), &["add", "local.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "local advance"]);
        git(tmp.path(), &["branch", "origin-side", base.as_str()]);
        git(tmp.path(), &["checkout", "origin-side"]);
        fs::write(tmp.path().join("remote.txt"), "remote\n").unwrap();
        git(tmp.path(), &["add", "remote.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "remote advance"]);
        let remote = current_commit(tmp.path(), "HEAD").unwrap();
        git(tmp.path(), &["checkout", "main"]);
        git(
            tmp.path(),
            &["update-ref", "refs/remotes/origin/main", remote.as_str()],
        );

        let selection = preferred_main_start_ref(tmp.path()).unwrap();
        assert_eq!(selection.ref_name, "main");
        assert_eq!(
            selection.fallback_reason.as_deref(),
            Some("stale_origin_fallback ahead=1 divergent origin_ahead=1")
        );
    }

    #[test]
    fn preferred_main_start_ref_falls_back_to_local_main_when_origin_missing() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let selection = preferred_main_start_ref(tmp.path()).unwrap();
        assert_eq!(selection.ref_name, "main");
        assert_eq!(
            selection.fallback_reason.as_deref(),
            Some("stale_origin_fallback ahead=0 origin_unreachable")
        );
    }

    #[test]
    fn ensure_worktree_branch_for_dispatch_from_trunk_resets_from_mainline() {
        let Some(tmp) = init_repo() else {
            return;
        };
        git(tmp.path(), &["checkout", "-b", "mainline"]);
        fs::write(tmp.path().join("mainline.txt"), "configured trunk\n").unwrap();
        git(tmp.path(), &["add", "mainline.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "mainline advance"]);

        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-1-500",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("work.txt"), "old work\n").unwrap();
        git(&wt_path, &["add", "work.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "old task"]);

        let reset =
            ensure_worktree_branch_for_dispatch_from_trunk(&wt_path, "eng-1-1-502", "mainline")
                .unwrap();
        let head = run_git(&wt_path, ["rev-parse", "HEAD"]).unwrap().stdout;
        let mainline = run_git(tmp.path(), ["rev-parse", "mainline"])
            .unwrap()
            .stdout;

        assert!(reset.changed);
        assert_eq!(reset.reset_reason, Some(WorktreeResetReason::CleanReset));
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-1-1-502");
        assert_eq!(head, mainline);

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-500"]);
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-502"]);
    }

    #[test]
    fn ensure_worktree_branch_for_dispatch_cleans_dirty_worktree_before_reset() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-1-500",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("scratch.txt"), "dirty\n").unwrap();
        git(&wt_path, &["add", "scratch.txt"]);

        let reset = ensure_worktree_branch_for_dispatch(&wt_path, "eng-1-1-502").unwrap();

        assert!(reset.changed);
        assert_eq!(
            reset.reset_reason,
            Some(WorktreeResetReason::PreservedBeforeReset)
        );
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-1-1-502");
        assert!(!wt_path.join("scratch.txt").exists());
        assert!(
            String::from_utf8_lossy(&run_git(&wt_path, ["status", "--porcelain"]).unwrap().stdout)
                .trim()
                .is_empty()
        );
        let preserved = run_git(tmp.path(), ["show", "eng-1-1-500:scratch.txt"]).unwrap();
        assert!(
            preserved.status.success(),
            "dirty file should be preserved on the previous branch"
        );
        assert_eq!(String::from_utf8_lossy(&preserved.stdout), "dirty\n");
        let old_branch_log =
            run_git(tmp.path(), ["log", "--oneline", "-1", "eng-1-1-500"]).unwrap();
        assert!(
            String::from_utf8_lossy(&old_branch_log.stdout)
                .contains("wip: auto-save before worktree reset"),
            "previous branch should record an auto-save commit"
        );

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-500"]);
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-502"]);
    }

    #[test]
    fn reset_worktree_to_base_archives_dirty_base_branch_before_reset() {
        let Some(tmp) = init_repo() else {
            return;
        };

        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-main/test-eng",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("scratch.txt"), "dirty\n").unwrap();
        git(&wt_path, &["add", "scratch.txt"]);

        let reason = reset_worktree_to_base_with_options(
            &wt_path,
            "eng-main/test-eng",
            "wip: auto-save before worktree reset [eng-main/test-eng]",
            Duration::from_secs(5),
            PreserveFailureMode::SkipReset,
        )
        .unwrap();

        assert_eq!(reason, WorktreeResetReason::PreservedBeforeReset);
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-main/test-eng");
        assert!(!wt_path.join("scratch.txt").exists());
        let preserved_branch = String::from_utf8_lossy(
            &run_git(
                tmp.path(),
                [
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/preserved/",
                ],
            )
            .unwrap()
            .stdout,
        )
        .trim()
        .to_string();
        assert!(
            preserved_branch.starts_with("preserved/eng-main-test-eng-"),
            "expected archived preserved branch, got: {preserved_branch}"
        );
        let preserved_file = run_git(
            tmp.path(),
            ["show", &format!("{preserved_branch}:scratch.txt")],
        )
        .unwrap();
        assert!(
            preserved_file.status.success(),
            "dirty file should be preserved on archived branch"
        );
        assert_eq!(String::from_utf8_lossy(&preserved_file.stdout), "dirty\n");

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-main/test-eng"]);
        let _ = run_git(tmp.path(), ["branch", "-D", preserved_branch.as_str()]);
    }

    // #659 coverage: destructive resets must archive commits ahead of their
    // reset target onto a `preserved/<slug>-<stamp>` branch rather than
    // discarding them.

    fn preserved_branches_for(repo_root: &Path, slug: &str) -> Vec<String> {
        let output = run_git(
            repo_root,
            [
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/preserved/",
            ],
        )
        .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_string)
            .filter(|r| r.starts_with(&format!("preserved/{slug}-")))
            .collect()
    }

    #[test]
    fn reset_worktree_to_base_archives_base_branch_commits_ahead() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-main/659-fix",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("feature.txt"), "committed\n").unwrap();
        git(&wt_path, &["add", "feature.txt"]);
        git(
            &wt_path,
            &["commit", "-q", "-m", "feature commit ahead of main"],
        );
        let pre_reset_tip =
            String::from_utf8_lossy(&run_git(&wt_path, ["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();

        let reason = reset_worktree_to_base_with_options(
            &wt_path,
            "eng-main/659-fix",
            "wip: auto-save before worktree reset",
            Duration::from_secs(5),
            PreserveFailureMode::SkipReset,
        )
        .unwrap();

        assert_eq!(reason, WorktreeResetReason::CleanReset);

        let preserved = preserved_branches_for(tmp.path(), "eng-main-659-fix");
        assert!(
            !preserved.is_empty(),
            "expected preserved branch for commits ahead, found none"
        );
        let archive_tip = String::from_utf8_lossy(
            &run_git(tmp.path(), ["rev-parse", preserved[0].as_str()])
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        assert_eq!(
            archive_tip, pre_reset_tip,
            "preserved branch must point at pre-reset tip"
        );
        let feature_on_archive = run_git(
            tmp.path(),
            ["show", &format!("{}:feature.txt", preserved[0])],
        )
        .unwrap();
        assert!(
            feature_on_archive.status.success(),
            "preserved branch should carry the committed file"
        );

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-main/659-fix"]);
        for p in preserved {
            let _ = run_git(tmp.path(), ["branch", "-D", p.as_str()]);
        }
    }

    #[test]
    fn reset_worktree_to_base_noop_archive_when_no_commits_ahead() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-main/clean",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );

        let reason = reset_worktree_to_base_with_options(
            &wt_path,
            "eng-main/clean",
            "wip: auto-save before worktree reset",
            Duration::from_secs(5),
            PreserveFailureMode::SkipReset,
        )
        .unwrap();

        assert_eq!(reason, WorktreeResetReason::CleanReset);
        let preserved = preserved_branches_for(tmp.path(), "eng-main-clean");
        assert!(
            preserved.is_empty(),
            "no archive should be created when branch has 0 commits ahead, got {preserved:?}"
        );

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-main/clean"]);
    }

    #[test]
    fn reset_worktree_to_base_if_clean_archives_commits_ahead() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-main/clean-ahead",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("ahead.txt"), "committed\n").unwrap();
        git(&wt_path, &["add", "ahead.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "committed work"]);

        let reason = reset_worktree_to_base_if_clean(
            &wt_path,
            "eng-main/clean-ahead",
            "test/reset-if-clean",
        )
        .unwrap();
        assert_eq!(reason, WorktreeResetReason::CleanReset);

        let preserved = preserved_branches_for(tmp.path(), "eng-main-clean-ahead");
        assert!(
            !preserved.is_empty(),
            "reset_worktree_to_base_if_clean must archive commits ahead"
        );
        let preserved_file =
            run_git(tmp.path(), ["show", &format!("{}:ahead.txt", preserved[0])]).unwrap();
        assert!(preserved_file.status.success());

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-main/clean-ahead"]);
        for p in preserved {
            let _ = run_git(tmp.path(), ["branch", "-D", p.as_str()]);
        }
    }

    #[test]
    fn ensure_worktree_branch_for_dispatch_archives_expected_branch_commits_ahead() {
        let Some(tmp) = init_repo() else {
            return;
        };
        // Create the target expected branch with commits ahead of main.
        git(tmp.path(), &["branch", "eng-1-1-659", "main"]);
        git(tmp.path(), &["checkout", "eng-1-1-659"]);
        fs::write(tmp.path().join("expected.txt"), "work\n").unwrap();
        git(tmp.path(), &["add", "expected.txt"]);
        git(
            tmp.path(),
            &["commit", "-q", "-m", "prior work on expected"],
        );
        git(tmp.path(), &["checkout", "main"]);

        // Create the worktree on a different branch (will need to be reset).
        let wt_path = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-1-500",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );

        let reset = ensure_worktree_branch_for_dispatch(&wt_path, "eng-1-1-659").unwrap();
        assert!(reset.changed);
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-1-1-659");

        let preserved = preserved_branches_for(tmp.path(), "eng-1-1-659");
        assert!(
            !preserved.is_empty(),
            "ensure_worktree_branch_for_dispatch must archive commits ahead on expected branch"
        );
        let preserved_file = run_git(
            tmp.path(),
            ["show", &format!("{}:expected.txt", preserved[0])],
        )
        .unwrap();
        assert!(
            preserved_file.status.success(),
            "preserved branch should carry the committed file on expected branch"
        );

        let _ = run_git(
            tmp.path(),
            ["worktree", "remove", "--force", wt_path.to_str().unwrap()],
        );
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-500"]);
        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-659"]);
        for p in preserved {
            let _ = run_git(tmp.path(), ["branch", "-D", p.as_str()]);
        }
    }

    #[test]
    fn archive_branch_if_commits_ahead_assigns_unique_names_for_concurrent_engineers() {
        let Some(tmp) = init_repo() else {
            return;
        };
        git(tmp.path(), &["checkout", "-b", "eng-1-1-999"]);
        fs::write(tmp.path().join("eng1.txt"), "eng1\n").unwrap();
        git(tmp.path(), &["add", "eng1.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "eng1 work"]);
        git(tmp.path(), &["checkout", "main"]);

        let first = archive_branch_if_commits_ahead(tmp.path(), "eng-1-1-999", "main", "test")
            .unwrap()
            .expect("first archive should succeed");
        let second = archive_branch_if_commits_ahead(tmp.path(), "eng-1-1-999", "main", "test")
            .unwrap()
            .expect("second archive should succeed with unique suffix");
        assert_ne!(
            first, second,
            "concurrent archives on the same tip must produce distinct branch names"
        );

        let _ = run_git(tmp.path(), ["branch", "-D", "eng-1-1-999"]);
        let _ = run_git(tmp.path(), ["branch", "-D", first.as_str()]);
        let _ = run_git(tmp.path(), ["branch", "-D", second.as_str()]);
    }

    #[test]
    fn archive_branch_if_commits_ahead_skips_missing_branch() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let result =
            archive_branch_if_commits_ahead(tmp.path(), "nonexistent-branch", "main", "test")
                .unwrap();
        assert!(result.is_none(), "missing branch should return None");
    }

    // ---------------------------------------------------------------------
    // B-1 multi-repo-aware helpers
    // ---------------------------------------------------------------------

    fn init_sub_repo(parent: &Path, name: &str) -> PathBuf {
        let sub = parent.join(name);
        fs::create_dir_all(&sub).unwrap();
        git(&sub, &["init", "-q", "-b", "mainline"]);
        git(&sub, &["config", "user.email", "batty-test@example.com"]);
        git(&sub, &["config", "user.name", "Batty Test"]);
        fs::write(sub.join("README.md"), format!("{name}\n")).unwrap();
        git(&sub, &["add", "README.md"]);
        git(&sub, &["commit", "-q", "-m", "init"]);
        // Set origin/HEAD so default_branch_name picks up mainline without a remote.
        // In tests we leave it unset and rely on the show-ref fallback probe.
        sub
    }

    #[test]
    fn iter_repos_for_mutation_returns_single_repo_for_standalone_worktree() {
        if !git_available() {
            return;
        }
        let Some(tmp) = init_repo() else {
            return;
        };
        let repos = iter_repos_for_mutation(tmp.path());
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0], tmp.path());
    }

    #[test]
    fn iter_repos_for_mutation_returns_sub_repos_for_multi_repo_container() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // Container dir is NOT a git repo, but holds 3 sub-repos.
        let r1 = init_sub_repo(tmp.path(), "PkgA");
        let r2 = init_sub_repo(tmp.path(), "PkgB");
        let r3 = init_sub_repo(tmp.path(), "PkgC");
        let mut repos = iter_repos_for_mutation(tmp.path());
        repos.sort();
        let mut expected = vec![r1, r2, r3];
        expected.sort();
        assert_eq!(repos, expected);
    }

    #[test]
    fn iter_repos_for_mutation_returns_empty_for_non_git_non_container() {
        let tmp = tempfile::tempdir().unwrap();
        // Empty dir: no .git, no git sub-dirs.
        let repos = iter_repos_for_mutation(tmp.path());
        assert!(repos.is_empty());
    }

    #[test]
    fn checkout_base_branch_across_repos_handles_container_with_no_git_no_fatal() {
        // B-1(1.1) regression: container-shaped worktree must NOT bail with
        // 'fatal: not a git repository'. The fix treats a no-sub-repo container
        // as a no-op. This was the exact shape that kept blocking #7 TTL reclaim.
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let result = checkout_base_branch_across_repos(tmp.path(), "eng-1-3/99", "main", "test");
        assert!(
            result.is_ok(),
            "container with no sub-repos must NOT fatal; got: {result:?}"
        );
    }

    #[test]
    fn checkout_base_branch_across_repos_switches_branch_in_each_sub_repo() {
        // B-1(1.2): three-sub-repo workspace must get the new branch created in
        // each repo. Verifies multi-repo fan-out works for the base-branch reset
        // that TTL reclaim runs.
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let r1 = init_sub_repo(tmp.path(), "PkgA");
        let r2 = init_sub_repo(tmp.path(), "PkgB");
        let r3 = init_sub_repo(tmp.path(), "PkgC");
        checkout_base_branch_across_repos(tmp.path(), "eng-1-3/19", "main", "test").unwrap();
        for repo in [&r1, &r2, &r3] {
            let out = run_git(repo, ["branch", "--show-current"]).unwrap();
            let branch = String::from_utf8_lossy(&out.stdout);
            assert_eq!(
                branch.trim(),
                "eng-1-3/19",
                "repo {} should be on new branch",
                repo.display()
            );
        }
    }

    fn worktree_on_branch_with_commit(repo: &Path, name: &str, branch: &str) -> PathBuf {
        let wt_path = repo.join(name);
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("work.txt"), format!("{branch} work\n")).unwrap();
        git(&wt_path, &["add", "work.txt"]);
        git(&wt_path, &["commit", "-q", "-m", "stray work"]);
        wt_path
    }

    fn list_branches(repo: &Path, pattern: &str) -> Vec<String> {
        let out = run_git(
            repo,
            ["branch", "--list", "--format=%(refname:short)", pattern],
        )
        .unwrap()
        .stdout;
        String::from_utf8_lossy(&out)
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn git_succeeds(repo: &Path, args: &[&str]) -> bool {
        run_git(repo, args).unwrap().status.success()
    }

    #[test]
    fn heal_stray_task_branch_carries_same_task_work_to_expected_branch() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = worktree_on_branch_with_commit(tmp.path(), "wt-heal", "eng-a/28");
        fs::write(wt_path.join("dirty.txt"), "uncommitted\n").unwrap();

        let archive =
            heal_stray_task_branch(&wt_path, "eng-b/28", "eng-main/eng-b", "main").unwrap();

        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-b/28");
        assert!(archive.is_some_and(|name| name.starts_with("preserved/eng-a-28-")));
        // Committed and dirty work from the stray branch carried over.
        assert!(git_succeeds(&wt_path, &["show", "HEAD:work.txt"]));
        assert!(git_succeeds(&wt_path, &["show", "HEAD:dirty.txt"]));
        assert!(!has_uncommitted_changes(&wt_path).unwrap());
    }

    #[test]
    fn heal_stray_task_branch_archives_foreign_branch_before_dispatch_reset() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = worktree_on_branch_with_commit(tmp.path(), "wt-foreign", "doc-1/3");

        let archive = heal_stray_task_branch(&wt_path, "eng-b/5", "eng-main/eng-b", "main")
            .unwrap()
            .expect("stray commits must be archived");
        assert!(git_succeeds(
            tmp.path(),
            &["show", &format!("{archive}:work.txt")]
        ));

        // The normal dispatch path can now switch to the member's own branch.
        let reset = ensure_worktree_branch_for_dispatch(&wt_path, "eng-b/5").unwrap();
        assert!(reset.changed);
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-b/5");
    }

    #[test]
    fn heal_stray_task_branch_is_noop_on_expected_branch() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = worktree_on_branch_with_commit(tmp.path(), "wt-noop", "eng-b/9");
        assert_eq!(
            heal_stray_task_branch(&wt_path, "eng-b/9", "eng-main/eng-b", "main").unwrap(),
            None
        );
        assert!(list_branches(tmp.path(), "preserved/*").is_empty());
    }

    #[test]
    fn recover_worktree_to_base_preserving_aborts_conflicted_merge_and_resets() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt_path = tmp.path().join("wt-conflict");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-b",
                "eng-1/7",
                wt_path.to_str().unwrap(),
                "main",
            ],
        );
        fs::write(wt_path.join("README.md"), "task change\n").unwrap();
        git(&wt_path, &["commit", "-q", "-am", "task change"]);
        fs::write(tmp.path().join("README.md"), "main change\n").unwrap();
        git(tmp.path(), &["commit", "-q", "-am", "main change"]);
        assert!(
            !git_succeeds(&wt_path, &["merge", "main"]),
            "setup requires a conflicted merge"
        );
        fs::write(wt_path.join("notes.txt"), "agent notes\n").unwrap();
        // The clean-only reset refuses this lane every time, which is what
        // made merge-conflict recovery loop forever.
        assert_eq!(
            reset_worktree_to_base_if_clean(&wt_path, "eng-main/eng-1", "test").unwrap(),
            WorktreeResetReason::PreserveFailedResetSkipped
        );

        let reason = recover_worktree_to_base_preserving(
            &wt_path,
            "eng-main/eng-1",
            "merge-conflict recovery",
            "main",
        )
        .unwrap();

        assert_eq!(reason, WorktreeResetReason::PreservedBeforeReset);
        assert_eq!(git_current_branch(&wt_path).unwrap(), "eng-main/eng-1");
        assert!(!crate::team::task_loop::git_has_unresolved_conflicts(&wt_path).unwrap());
        assert!(!has_uncommitted_changes(&wt_path).unwrap());
        // Dirty notes were auto-saved on the task branch and archived.
        assert!(git_succeeds(tmp.path(), &["show", "eng-1/7:notes.txt"]));
        assert!(!list_branches(tmp.path(), "preserved/eng-1-7-*").is_empty());
    }

    // --- archive failures abort destructive resets --------------------------------

    #[test]
    fn archive_branch_if_commits_ahead_fails_closed_on_bad_base_ref() {
        let Some(tmp) = init_repo() else {
            return;
        };
        git(tmp.path(), &["branch", "eng-1"]);
        assert!(
            archive_branch_if_commits_ahead(tmp.path(), "eng-1", "no-such-base", "test").is_err()
        );
    }

    #[test]
    fn reset_aborts_without_rewriting_branch_when_archive_fails() {
        let Some(tmp) = init_repo() else {
            return;
        };
        git(tmp.path(), &["checkout", "-q", "-b", "eng-main/eng-1"]);
        fs::write(tmp.path().join("work.txt"), "unmerged\n").unwrap();
        git(tmp.path(), &["add", "work.txt"]);
        git(tmp.path(), &["commit", "-q", "-m", "unmerged work"]);
        let tip_before = rev_parse(tmp.path(), "eng-main/eng-1");

        // The trunk ref cannot be resolved, so commits ahead cannot be counted.
        let result = reset_worktree_to_base_if_clean_from_trunk(
            tmp.path(),
            "eng-main/eng-1",
            "test",
            "no-such-trunk",
        );

        assert!(result.is_err(), "reset must abort: {result:?}");
        assert_eq!(rev_parse(tmp.path(), "eng-main/eng-1"), tip_before);
    }

    fn rev_parse(repo: &Path, rev: &str) -> String {
        let output = run_git(repo, ["rev-parse", rev]).unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    // --- force-reset fallback snapshots first -------------------------------------

    #[test]
    fn snapshot_dirty_worktree_captures_tracked_and_untracked_without_touching_index() {
        let Some(tmp) = init_repo() else {
            return;
        };
        fs::write(tmp.path().join("README.md"), "edited\n").unwrap();
        fs::write(tmp.path().join("staged.txt"), "staged\n").unwrap();
        git(tmp.path(), &["add", "staged.txt"]);
        fs::write(tmp.path().join("untracked.txt"), "untracked\n").unwrap();
        let head_before = rev_parse(tmp.path(), "HEAD");

        let snapshot = snapshot_dirty_worktree_to_ref(tmp.path(), "test").unwrap();

        assert!(snapshot.starts_with("refs/heads/preserved/main-snapshot-"));
        let show = |path: &str| {
            let output =
                run_git(tmp.path(), ["show", format!("{snapshot}:{path}").as_str()]).unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        assert_eq!(show("README.md"), "edited\n");
        assert_eq!(show("staged.txt"), "staged\n");
        assert_eq!(show("untracked.txt"), "untracked\n");
        assert_eq!(rev_parse(tmp.path(), &format!("{snapshot}^")), head_before);
        // Real HEAD and index are unchanged.
        assert_eq!(rev_parse(tmp.path(), "HEAD"), head_before);
        let cached = run_git(tmp.path(), ["diff", "--cached", "--name-only"]).unwrap();
        assert_eq!(String::from_utf8_lossy(&cached.stdout).trim(), "staged.txt");
    }

    #[test]
    fn force_reset_after_failed_preserve_keeps_a_snapshot() {
        let Some(tmp) = init_repo() else {
            return;
        };
        fs::write(tmp.path().join("README.md"), "precious\n").unwrap();
        fs::write(tmp.path().join("new.txt"), "precious new\n").unwrap();

        // A zero timeout makes the preserve commit fail.
        let reason = prepare_worktree_for_reset(
            tmp.path(),
            "wip: test",
            Duration::ZERO,
            PreserveFailureMode::ForceReset,
            "test",
        )
        .unwrap();

        assert_eq!(reason, WorktreeResetReason::PreserveFailedForceReset);
        assert!(
            !tmp.path().join("new.txt").exists(),
            "worktree was force-reset"
        );
        let refs = run_git(
            tmp.path(),
            [
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/preserved/",
            ],
        )
        .unwrap();
        let refs = String::from_utf8_lossy(&refs.stdout).into_owned();
        let snapshot = refs.lines().next().expect("snapshot ref created");
        let output = run_git(tmp.path(), ["show", format!("{snapshot}:new.txt").as_str()]).unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout), "precious new\n");
    }

    #[test]
    fn force_reset_is_refused_when_snapshot_fails() {
        let Some(tmp) = init_repo() else {
            return;
        };
        // A branch literally named `preserved` blocks creating any
        // `refs/heads/preserved/...` snapshot ref (directory/file conflict).
        git(tmp.path(), &["branch", "preserved"]);
        fs::write(tmp.path().join("new.txt"), "precious\n").unwrap();

        let result = prepare_worktree_for_reset(
            tmp.path(),
            "wip: test",
            Duration::ZERO,
            PreserveFailureMode::ForceReset,
            "test",
        );

        assert!(result.is_err(), "reset must be refused: {result:?}");
        assert_eq!(
            fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "precious\n"
        );
    }

    // --- worktree registration with symlinked paths ----------------------------------

    #[test]
    fn worktree_registered_matches_symlinked_path_spelling() {
        let Some(tmp) = init_repo() else {
            return;
        };
        let wt = tmp.path().join("wt");
        git(
            tmp.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt-branch",
                wt.to_str().unwrap(),
            ],
        );
        let link_parent = tempfile::tempdir().unwrap();
        let link = link_parent.path().join("repo-link");
        std::os::unix::fs::symlink(tmp.path(), &link).unwrap();

        assert!(worktree_registered(tmp.path(), &wt).unwrap());
        assert!(worktree_registered(tmp.path(), &link.join("wt")).unwrap());
        assert!(!worktree_registered(tmp.path(), &tmp.path().join("other")).unwrap());
    }
}
