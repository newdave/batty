//! Consolidated worktree operations: dirty-path inspection, commit counting,
//! preserve-by-commit, archive-branch naming and the single
//! [`reset_to_base`] entry point that every reset path goes through.
//!
//! All git invocations use the shared runner in [`crate::git`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use super::{PreserveFailureMode, WorktreeResetReason, git_current_branch};
use crate::team::git_cmd;
use crate::team::retry::{RetryConfig, retry_sync};

/// Batty-managed runtime directories that never count as user work.
pub(crate) const BATTY_RUNTIME_DIRS: &[&str] = &[".batty", ".cargo", ".batty-target"];

/// Prefixes of branches created by Batty to archive work before a destructive
/// operation. `legacy/` and `backup/` are older schemes that are still
/// recognised when reading existing refs.
pub(crate) const ARCHIVE_BRANCH_PREFIXES: &[&str] = &["preserved/", "legacy/", "backup/"];

/// Which paths [`dirty_paths`] ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExcludePolicy {
    /// Report every change `git status` shows.
    Nothing,
    /// Ignore Batty runtime dirs ([`BATTY_RUNTIME_DIRS`]).
    BattyRuntime,
}

/// How [`reset_to_base`] treats a dirty worktree and what it archives.
#[derive(Debug, Clone)]
pub(crate) struct ResetPolicy<'a> {
    /// Subsystem label used in audit logs and archive diagnostics.
    pub subsystem: &'a str,
    pub dirty: DirtyPolicy<'a>,
    /// Archive commits on the branch currently checked out (when it is not
    /// the base branch) before switching away from it.
    pub archive_current_branch: bool,
}

#[derive(Debug, Clone)]
pub(crate) enum DirtyPolicy<'a> {
    /// Leave a dirty worktree untouched (`PreserveFailedResetSkipped`).
    Refuse,
    /// Auto-save dirty work as a commit first; `on_failure` decides whether a
    /// failed save skips the reset or snapshots and force-resets.
    Preserve {
        commit_message: &'a str,
        timeout: Duration,
        on_failure: PreserveFailureMode,
    },
}

/// The `eng-main/<engineer>` base branch an engineer's worktree rests on.
pub(crate) fn engineer_base_branch_name(engineer_name: &str) -> String {
    format!("eng-main/{engineer_name}")
}

/// The `<engineer>/<task_id>` branch a task is dispatched on.
pub(crate) fn engineer_task_branch_name(
    engineer_name: &str,
    task_id: impl std::fmt::Display,
) -> String {
    format!("{engineer_name}/{task_id}")
}

/// Whether `branch` is the engineer's base branch.
pub(crate) fn is_engineer_base_branch(branch: &str) -> bool {
    branch.starts_with("eng-main/")
}

/// Whether `branch` is one of `engineer_name`'s `<engineer>/...` task branches.
pub(crate) fn is_engineer_task_branch(branch: &str, engineer_name: &str) -> bool {
    branch
        .strip_prefix(engineer_name)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Whether `branch` belongs to `engineer_name`: its base branch, a task
/// branch, or the legacy bare `<engineer>` branch.
pub(crate) fn branch_belongs_to_engineer(branch: &str, engineer_name: &str) -> bool {
    branch == engineer_name
        || branch == engineer_base_branch_name(engineer_name)
        || is_engineer_task_branch(branch, engineer_name)
}

/// Whether `branch` is a Batty archive branch (any naming generation).
pub(crate) fn is_archive_branch(branch: &str) -> bool {
    ARCHIVE_BRANCH_PREFIXES
        .iter()
        .any(|prefix| branch.starts_with(prefix))
}

fn retry_git<T, F>(operation: F) -> std::result::Result<T, git_cmd::GitError>
where
    F: Fn() -> std::result::Result<T, git_cmd::GitError>,
{
    retry_sync(&RetryConfig::fast(), operation)
}

fn status_args(policy: ExcludePolicy) -> Vec<String> {
    let mut args = vec![
        "status".to_string(),
        "--porcelain=v1".to_string(),
        "--untracked-files=all".to_string(),
    ];
    if policy == ExcludePolicy::BattyRuntime {
        args.extend(["--".to_string(), ".".to_string()]);
        args.extend(
            BATTY_RUNTIME_DIRS
                .iter()
                .map(|dir| format!(":(exclude){dir}")),
        );
    }
    args
}

/// Paths with uncommitted changes (staged, unstaged or untracked) in a
/// worktree. For a multi-repo container (not itself a repo, holding git
/// sub-repos) the paths of every sub-repo are aggregated as `<repo>/<path>`.
pub(crate) fn dirty_paths(worktree_dir: &Path, policy: ExcludePolicy) -> Result<Vec<String>> {
    let args = status_args(policy);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    if !git_cmd::is_git_repo(worktree_dir) {
        let sub_repos = git_cmd::discover_sub_repos(worktree_dir);
        if !sub_repos.is_empty() {
            let mut aggregated = Vec::new();
            for repo in sub_repos {
                let status = retry_git(|| git_cmd::run_git(&repo, &args).map(|out| out.stdout))
                    .map_err(|error| {
                        anyhow::anyhow!("failed to inspect sub-repo worktree status: {error}")
                    })?;
                let repo_name = repo
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                for path in parse_status_paths(&status) {
                    if repo_name.is_empty() {
                        aggregated.push(path);
                    } else {
                        aggregated.push(format!("{repo_name}/{path}"));
                    }
                }
            }
            return Ok(aggregated);
        }
        // Not a git repo and no git sub-repos: fall through so the status
        // call surfaces the original error.
    }
    let status = retry_git(|| git_cmd::run_git(worktree_dir, &args).map(|out| out.stdout))
        .map_err(|error| anyhow::anyhow!("failed to inspect worktree status: {error}"))?;
    Ok(parse_status_paths(&status))
}

/// Whether [`dirty_paths`] is non-empty.
pub(crate) fn is_dirty(worktree_dir: &Path, policy: ExcludePolicy) -> Result<bool> {
    Ok(!dirty_paths(worktree_dir, policy)?.is_empty())
}

/// Paths currently staged in the index.
pub(crate) fn staged_paths(worktree_dir: &Path) -> Result<Vec<String>> {
    let output = retry_git(|| git_cmd::run_git(worktree_dir, &["diff", "--cached", "--name-only"]))
        .map_err(|error| anyhow::anyhow!("failed to inspect staged worktree changes: {error}"))?;
    Ok(output
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn parse_status_paths(status: &str) -> Vec<String> {
    status
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let path = line[3..].trim();
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect()
}

/// `git rev-list --count <range>`.
pub(crate) fn rev_count(repo: &Path, range: &str) -> Result<u32> {
    let output = crate::git::cmd(repo)
        .args(["rev-list", "--count", range])
        .run()
        .with_context(|| format!("failed to count commits in {} for {range}", repo.display()))?;
    let stdout = output.stdout_trimmed();
    stdout
        .parse::<u32>()
        .with_context(|| format!("failed to parse git rev-list --count {range} output: {stdout:?}"))
}

/// Number of commits reachable from `head` but not from `base`.
pub(crate) fn ahead_count(repo: &Path, base: &str, head: &str) -> Result<u32> {
    rev_count(repo, &format!("{base}..{head}"))
}

/// Whether a fully-qualified ref (e.g. `refs/heads/x`) exists. Errors only
/// when git itself fails (not a repo, corrupt refs, ...).
pub(crate) fn ref_exists(repo: &Path, ref_name: &str) -> Result<bool> {
    let output = crate::git::cmd(repo)
        .args(["show-ref", "--verify", "--quiet", ref_name])
        .allow_fail()
        .run()
        .with_context(|| format!("failed to inspect {ref_name} in {}", repo.display()))?;
    match output.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "failed to inspect {} in {}: {}",
            ref_name,
            repo.display(),
            output.stderr_trimmed()
        ),
    }
}

/// The first free archive branch name for `label`:
/// `preserved/<label-slug>-<unix-secs>`, then `...-1`, `...-2`, ...
pub(crate) fn free_archive_branch_name(repo: &Path, label: &str) -> Result<String> {
    let slug = label.replace('/', "-");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut archive = format!("preserved/{slug}-{stamp}");
    let mut suffix: u32 = 0;
    while ref_exists(repo, &format!("refs/heads/{archive}"))? {
        suffix += 1;
        if suffix > 100 {
            bail!(
                "exhausted preserve-branch suffixes for '{}' in {}",
                label,
                repo.display()
            );
        }
        archive = format!("preserved/{slug}-{stamp}-{suffix}");
    }
    Ok(archive)
}

/// Create an archive branch ([`free_archive_branch_name`]) pointing at
/// `source_rev`. Returns the new branch name.
pub(crate) fn archive_branch(repo: &Path, source_rev: &str, label: &str) -> Result<String> {
    let archive = free_archive_branch_name(repo, label)?;
    crate::git::cmd(repo)
        .args(["branch", archive.as_str(), source_rev])
        .run()
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to archive '{}' as '{}' in {}: {}",
                source_rev,
                archive,
                repo.display(),
                error.detail()
            )
        })?;
    Ok(archive)
}

/// [`archive_branch`], but reuse an existing `preserved/<label-slug>-*`
/// branch that already points at the same commit. Used when trunk is missing
/// and every recovery retry would otherwise add another identical archive.
pub(crate) fn archive_branch_reusing(repo: &Path, source_rev: &str, label: &str) -> Result<String> {
    let slug = label.replace('/', "-");
    let tip = crate::git::cmd(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{source_rev}^{{commit}}"))
        .stdout()
        .with_context(|| format!("failed to resolve '{source_rev}' in {}", repo.display()))?;
    let existing = crate::git::cmd(repo)
        .args(["for-each-ref", "--format=%(objectname) %(refname:short)"])
        .arg(format!("refs/heads/preserved/{slug}-*"))
        .stdout()
        .with_context(|| format!("failed to list archive branches in {}", repo.display()))?;
    // `preserved/a-*` also matches `preserved/a-b-123`; the tail after the
    // slug must be the stamp (digits) plus an optional numeric suffix.
    let prefix = format!("preserved/{slug}-");
    let reusable = existing.lines().find_map(|line| {
        let (oid, name) = line.split_once(' ')?;
        let tail = name.strip_prefix(&prefix)?;
        let numeric = !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit() || c == '-');
        (oid == tip && numeric).then(|| name.to_string())
    });
    match reusable {
        Some(name) => Ok(name),
        None => archive_branch(repo, source_rev, label),
    }
}

/// Archive `branch_name` (see [`archive_branch`]) when it has commits ahead
/// of `base_ref`. Returns `Ok(None)` when the branch is missing or has no
/// commits ahead. Fails closed: if the ahead count cannot be determined the
/// caller must not rewrite the branch.
///
/// Used to preserve engineer work before any destructive `git checkout -B
/// <branch> <base>` that would rewrite the branch ref. See issue #659.
pub(crate) fn archive_branch_if_commits_ahead(
    worktree_path: &Path,
    branch_name: &str,
    base_ref: &str,
    subsystem: &str,
) -> Result<Option<String>> {
    if !ref_exists(worktree_path, &format!("refs/heads/{branch_name}"))? {
        return Ok(None);
    }
    let ahead = ahead_count(worktree_path, base_ref, branch_name).with_context(|| {
        format!(
            "failed to count commits on '{}' ahead of '{}' in {} before archiving",
            branch_name,
            base_ref,
            worktree_path.display()
        )
    })?;
    if ahead == 0 {
        return Ok(None);
    }
    let archive = archive_branch(worktree_path, branch_name, branch_name)?;
    info!(
        subsystem,
        worktree = %worktree_path.display(),
        branch = branch_name,
        base_ref,
        commits_ahead = ahead,
        archive = %archive,
        "archived branch commits ahead of base before destructive reset (#659)"
    );
    Ok(Some(archive))
}

/// Archive whatever is checked out before switching away from it: the
/// current branch's commits ahead of trunk, or a detached HEAD that is ahead
/// of trunk (labelled `detached_label`).
fn archive_checked_out_work(
    worktree_path: &Path,
    subsystem: &str,
    trunk_branch: &str,
    detached_label: &str,
) -> Result<Option<String>> {
    archive_checked_out_work_except(worktree_path, subsystem, trunk_branch, detached_label, None)
}

/// [`archive_checked_out_work`], leaving any repo that is on `skip_branch`
/// alone (the caller archives that branch itself).
fn archive_checked_out_work_except(
    worktree_path: &Path,
    subsystem: &str,
    trunk_branch: &str,
    detached_label: &str,
    skip_branch: Option<&str>,
) -> Result<Option<String>> {
    let mut first = None;
    for repo in repos_for_mutation(worktree_path) {
        let trunk = effective_trunk_branch_for_repo(&repo, trunk_branch);
        let branch = git_current_branch(&repo).ok();
        if skip_branch.is_some() && branch.as_deref() == skip_branch {
            continue;
        }
        let trunk_present = commit_resolves(&repo, &trunk)?;
        let archive = match (branch.as_deref(), trunk_present) {
            (Some(branch), true) => {
                archive_branch_if_commits_ahead(&repo, branch, &trunk, subsystem)?
            }
            (Some(branch), false) => {
                if ref_exists(&repo, &format!("refs/heads/{branch}"))? {
                    let archive = archive_branch_reusing(&repo, branch, branch)?;
                    warn!(
                        subsystem,
                        repo = %repo.display(),
                        branch,
                        trunk = %trunk,
                        archive = %archive,
                        "trunk ref missing; archived checked-out branch unconditionally"
                    );
                    Some(archive)
                } else {
                    None
                }
            }
            (None, true) if ahead_count(&repo, &trunk, "HEAD")? > 0 => {
                Some(archive_branch(&repo, "HEAD", detached_label)?)
            }
            (None, true) => None,
            (None, false) if commit_resolves(&repo, "HEAD")? => {
                let archive = archive_branch_reusing(&repo, "HEAD", detached_label)?;
                warn!(
                    subsystem,
                    repo = %repo.display(),
                    trunk = %trunk,
                    archive = %archive,
                    "trunk ref missing; archived detached HEAD unconditionally"
                );
                Some(archive)
            }
            (None, false) => None,
        };
        if first.is_none() {
            first = archive;
        }
    }
    Ok(first)
}

/// Whether `rev` resolves to a commit in `repo`.
fn commit_resolves(repo: &Path, rev: &str) -> Result<bool> {
    Ok(crate::git::cmd(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .succeeds()?)
}

pub(crate) fn log_mutation_audit(
    worktree_dir: &Path,
    subsystem: &str,
    operation: &str,
    files: &[String],
) {
    let branch = git_current_branch(worktree_dir)
        .unwrap_or_else(|_| "<detached-or-unavailable>".to_string());
    let file_summary = if files.is_empty() {
        "<none>".to_string()
    } else {
        files.join(", ")
    };
    info!(
        subsystem,
        operation,
        cwd = %worktree_dir.display(),
        branch,
        files = %file_summary,
        "worktree mutation audit"
    );
}

/// `-c` overrides that give git a placeholder identity when the repo has none
/// configured, so preservation commits cannot fail with "Please tell me who
/// you are". Returns an empty slice when git can already resolve an identity.
pub(crate) fn fallback_identity_args(dir: &Path) -> &'static [&'static str] {
    let has_identity = ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"]
        .iter()
        .all(|var| {
            crate::git::cmd(dir)
                .args(["var", var])
                .succeeds()
                .unwrap_or(false)
        });
    if has_identity {
        &[]
    } else {
        &["-c", "user.name=batty", "-c", "user.email=batty@localhost"]
    }
}

/// Run one preservation git step bounded by `timeout` (the runner kills the
/// whole process group when it expires).
fn run_bounded(worktree_dir: &Path, args: &[&str], timeout: Duration) -> Result<()> {
    let command = args.join(" ");
    match crate::git::cmd(worktree_dir)
        .args(args)
        .timeout(timeout)
        .run()
    {
        Ok(_) => Ok(()),
        Err(crate::git::GitError::Timeout { .. }) => bail!(
            "`git {}` timed out after {}s in {}",
            command,
            timeout.as_secs(),
            worktree_dir.display()
        ),
        Err(crate::git::GitError::Failed { status, stderr, .. }) => {
            let stderr = stderr.trim();
            if stderr.is_empty() {
                bail!(
                    "`git {}` failed in {} with status {}",
                    command,
                    worktree_dir.display(),
                    status
                )
            }
            bail!(
                "`git {}` failed in {} with status {}: {}",
                command,
                worktree_dir.display(),
                status,
                stderr
            )
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "failed to launch `git {}` in {}",
                command,
                worktree_dir.display()
            )
        }),
    }
}

fn rollback_newly_staged_paths(
    worktree_dir: &Path,
    original_staged_paths: &HashSet<String>,
    subsystem: &str,
) {
    let rollback_paths = match staged_paths(worktree_dir) {
        Ok(paths) => paths
            .into_iter()
            .filter(|path| !original_staged_paths.contains(path))
            .collect::<Vec<_>>(),
        Err(error) => {
            warn!(
                subsystem,
                cwd = %worktree_dir.display(),
                error = %error,
                "failed to inspect staged paths while rolling back preservation staging"
            );
            return;
        }
    };

    if rollback_paths.is_empty() {
        return;
    }

    log_mutation_audit(worktree_dir, subsystem, "git reset -q --", &rollback_paths);
    match crate::git::cmd(worktree_dir)
        .args(["reset", "-q", "--"])
        .args(&rollback_paths)
        .run()
    {
        Ok(_) => {}
        Err(error @ crate::git::GitError::Failed { .. }) => warn!(
            subsystem,
            cwd = %worktree_dir.display(),
            stderr = %error.stderr().trim(),
            "failed to roll back newly staged paths after preserve failure"
        ),
        Err(error) => warn!(
            subsystem,
            cwd = %worktree_dir.display(),
            error = %error,
            "failed to launch git while rolling back preservation staging"
        ),
    }
}

/// Commit every user change in `worktree_dir` (excluding Batty runtime dirs)
/// as `commit_message`. Returns `Ok(false)` when there was nothing to save.
/// Each git step is bounded by `timeout`; on failure, paths this call staged
/// are unstaged again.
pub(crate) fn preserve_with_commit(
    worktree_dir: &Path,
    commit_message: &str,
    timeout: Duration,
    subsystem: &str,
) -> Result<bool> {
    let dirty = dirty_paths(worktree_dir, ExcludePolicy::BattyRuntime)?;
    if dirty.is_empty() {
        return Ok(false);
    }

    // Stage from the repository root, then unstage Batty-managed runtime dirs.
    // Explicit `:(exclude)` pathspecs against ignored dirs can make `git add`
    // fail before it stages the real user changes we need to preserve.
    let original_staged_paths = staged_paths(worktree_dir)?
        .into_iter()
        .collect::<HashSet<_>>();
    let mut unstage_args = vec!["reset", "-q", "--"];
    unstage_args.extend(BATTY_RUNTIME_DIRS);
    let preserve_result = (|| -> Result<bool> {
        log_mutation_audit(worktree_dir, subsystem, "git add -A", &dirty);
        run_bounded(worktree_dir, &["add", "-A"], timeout)?;
        run_bounded(worktree_dir, &unstage_args, timeout)?;

        let staged = staged_paths(worktree_dir)?;
        if staged.is_empty() {
            return Ok(false);
        }

        log_mutation_audit(worktree_dir, subsystem, "git commit", &staged);
        let mut commit_args: Vec<&str> = fallback_identity_args(worktree_dir).to_vec();
        commit_args.extend([
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-m",
            commit_message,
        ]);
        run_bounded(worktree_dir, &commit_args, timeout)?;
        Ok(true)
    })();

    if preserve_result.is_err() {
        rollback_newly_staged_paths(worktree_dir, &original_staged_paths, subsystem);
    }

    preserve_result
}

/// Save every uncommitted change in `worktree_path` (staged, unstaged and
/// untracked, minus ignored files and Batty runtime dirs) as a commit on a
/// new `refs/heads/preserved/<branch>-snapshot-<stamp>` ref, without touching
/// the real index, worktree or HEAD. Returns the created ref name.
///
/// Used before a forced reset when the normal preserve commit failed, so the
/// reset can never be the only copy of the work.
pub(crate) fn snapshot_dirty_worktree_to_ref(
    worktree_path: &Path,
    subsystem: &str,
) -> Result<String> {
    fn git_checked(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
        let mut command = crate::git::cmd(dir).args(args);
        if let Some(index) = index {
            command = command.env("GIT_INDEX_FILE", index);
        }
        let output = command.run().map_err(|error| match &error {
            crate::git::GitError::Failed { stderr, .. } => anyhow::anyhow!(
                "`git {}` failed in {}: {}",
                args.join(" "),
                dir.display(),
                stderr.trim()
            ),
            _ => anyhow::Error::new(error)
                .context(format!("failed to execute `git {}`", args.join(" "))),
        })?;
        Ok(output.stdout_trimmed())
    }

    let real_index = PathBuf::from(git_checked(
        worktree_path,
        None,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_index = real_index.with_file_name(format!(
        "index.batty-snapshot-{}-{stamp}",
        std::process::id()
    ));
    // Remove the temp index on every exit path.
    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = RemoveOnDrop(temp_index.clone());
    if real_index.exists() {
        std::fs::copy(&real_index, &temp_index).with_context(|| {
            format!("failed to copy index {} for snapshot", real_index.display())
        })?;
    }

    let index = Some(temp_index.as_path());
    let mut unstage_args = vec!["reset", "-q", "--"];
    unstage_args.extend(BATTY_RUNTIME_DIRS);
    git_checked(worktree_path, index, &["add", "-A"])?;
    git_checked(worktree_path, index, &unstage_args)?;
    let tree = git_checked(worktree_path, index, &["write-tree"])?;
    let head = git_checked(
        worktree_path,
        None,
        &["rev-parse", "--verify", "-q", "HEAD"],
    )
    .ok();
    let branch = git_current_branch(worktree_path).unwrap_or_else(|_| "detached".to_string());
    let message = format!("wip: batty snapshot before forced reset ({subsystem}) [{branch}]");

    let mut commit_args: Vec<&str> = fallback_identity_args(worktree_path).to_vec();
    commit_args.extend(["commit-tree", tree.as_str(), "-m", message.as_str()]);
    if let Some(head) = head.as_deref() {
        commit_args.extend(["-p", head]);
    }
    let commit = git_checked(worktree_path, None, &commit_args)?;

    let slug = branch.replace('/', "-");
    let ref_name = format!("refs/heads/preserved/{slug}-snapshot-{stamp}");
    // Empty old-value: create only, never overwrite an existing ref.
    git_checked(
        worktree_path,
        None,
        &["update-ref", "-m", "batty snapshot", &ref_name, &commit, ""],
    )?;
    Ok(ref_name)
}

/// Best-effort `git <args>` whose outcome does not matter (aborts of
/// operations that may not be in progress).
fn run_best_effort(worktree_path: &Path, args: &[&str]) {
    let _ = crate::git::cmd(worktree_path).args(args).allow_fail().run();
}

/// Abort any in-progress merge, rebase or cherry-pick. Each abort is a no-op
/// when that operation is not in progress.
pub(crate) fn abort_in_progress_git_operations(worktree_path: &Path) {
    for op in ["merge", "rebase", "cherry-pick"] {
        run_best_effort(worktree_path, &[op, "--abort"]);
    }
}

/// Make a worktree safe to reset: auto-save dirty work as a commit, or —
/// when that fails and `preserve_failure_mode` is `ForceReset` — snapshot the
/// dirty tree to a ref and hard-reset. Never discards work without a copy.
pub(crate) fn prepare_worktree_for_reset(
    worktree_path: &Path,
    commit_message: &str,
    timeout: Duration,
    preserve_failure_mode: PreserveFailureMode,
    subsystem: &str,
) -> Result<WorktreeResetReason> {
    if !worktree_path.exists() || !is_dirty(worktree_path, ExcludePolicy::BattyRuntime)? {
        run_best_effort(worktree_path, &["merge", "--abort"]);
        return Ok(WorktreeResetReason::CleanReset);
    }

    match preserve_with_commit(worktree_path, commit_message, timeout, subsystem) {
        Ok(true) => {
            run_best_effort(worktree_path, &["merge", "--abort"]);
            return Ok(WorktreeResetReason::PreservedBeforeReset);
        }
        Ok(false) => {
            run_best_effort(worktree_path, &["merge", "--abort"]);
            return Ok(WorktreeResetReason::CleanReset);
        }
        Err(error) => {
            warn!(
                worktree = %worktree_path.display(),
                error = %error,
                "failed to preserve worktree before reset"
            );
        }
    }

    if preserve_failure_mode == PreserveFailureMode::SkipReset {
        return Ok(WorktreeResetReason::PreserveFailedResetSkipped);
    }

    // Never discard work without a saved copy: snapshot the dirty tree
    // (tracked + untracked) to a ref first, and refuse to reset if that fails.
    let snapshot_ref = snapshot_dirty_worktree_to_ref(worktree_path, subsystem)
        .with_context(|| {
            format!(
                "refusing to force-reset {}: preserve failed and the dirty-tree snapshot also failed",
                worktree_path.display()
            )
        })?;
    warn!(
        subsystem,
        worktree = %worktree_path.display(),
        snapshot_ref = %snapshot_ref,
        "preserve commit failed; saved dirty worktree snapshot before force-reset"
    );

    run_best_effort(worktree_path, &["merge", "--abort"]);
    log_mutation_audit(
        worktree_path,
        subsystem,
        "git reset --hard",
        &dirty_paths(worktree_path, ExcludePolicy::BattyRuntime).unwrap_or_default(),
    );
    crate::git::cmd(worktree_path)
        .args(["reset", "--hard"])
        .run()
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to force-reset worktree {}: {}",
                worktree_path.display(),
                error.detail()
            )
        })?;
    crate::git::cmd(worktree_path)
        .args(["clean", "-fd", "--exclude=.batty/"])
        .run()
        .map_err(|error| {
            anyhow::anyhow!(
                "failed to clean worktree {}: {}",
                worktree_path.display(),
                error.detail()
            )
        })?;

    Ok(WorktreeResetReason::PreserveFailedForceReset)
}

/// Reset `worktree_path` onto `base_branch` recreated from `trunk_branch`
/// (`git checkout -B <base> <trunk>` in every repo of the worktree).
///
/// Every reset path funnels through here. In order it:
/// 1. applies the dirty policy (refuse, or preserve-by-commit with a
///    skip/force fallback), returning early when the reset must not happen;
/// 2. optionally archives the checked-out branch (or detached HEAD);
/// 3. archives `base_branch` commits ahead of trunk; an archive failure
///    aborts the reset;
/// 4. recreates `base_branch` from trunk.
pub(crate) fn reset_to_base(
    worktree_path: &Path,
    base_branch: &str,
    trunk_branch: &str,
    policy: &ResetPolicy<'_>,
) -> Result<WorktreeResetReason> {
    let subsystem = policy.subsystem;
    let current_branch = git_current_branch(worktree_path).ok();

    let reason = match &policy.dirty {
        DirtyPolicy::Refuse => {
            let dirty = dirty_paths(worktree_path, ExcludePolicy::BattyRuntime).unwrap_or_default();
            if !dirty.is_empty() {
                warn!(
                    subsystem,
                    worktree = %worktree_path.display(),
                    branch = current_branch.as_deref().unwrap_or("<detached-or-unavailable>"),
                    files = %dirty.join(", "),
                    "skipping background worktree reset because the lane is dirty"
                );
                return Ok(WorktreeResetReason::PreserveFailedResetSkipped);
            }
            run_best_effort(worktree_path, &["merge", "--abort"]);
            WorktreeResetReason::CleanReset
        }
        DirtyPolicy::Preserve {
            commit_message,
            timeout,
            on_failure,
        } => prepare_worktree_for_reset(
            worktree_path,
            commit_message,
            *timeout,
            *on_failure,
            subsystem,
        )?,
    };
    if !reason.reset_performed() {
        return Ok(reason);
    }

    if policy.archive_current_branch {
        match current_branch.as_deref() {
            // The base branch itself is archived below.
            Some(branch) if branch == base_branch => {}
            _ => {
                // The current branch ref survives `checkout -B base`, but
                // archive anyway so a later cleanup of it cannot drop work.
                archive_checked_out_work_except(
                    worktree_path,
                    subsystem,
                    trunk_branch,
                    "detached",
                    Some(base_branch),
                )?;
            }
        }
    }

    // #659: guard the destructive `checkout -B <base_branch> <trunk>` by
    // archiving commits already on `base_branch` that are ahead of trunk
    // (including a WIP commit just preserved on it).
    archive_branch_if_commits_ahead_across_repos(
        worktree_path,
        base_branch,
        trunk_branch,
        subsystem,
    )?;
    checkout_base_branch_across_repos(worktree_path, base_branch, trunk_branch, subsystem)?;
    Ok(reason)
}

/// Self-heal a member worktree that is checked out on a branch other than
/// `expected_branch` (another member's branch, a stale task branch, or a
/// detached HEAD) before dispatching `expected_branch`.
///
/// Dirty work is auto-saved on the stray branch and the stray branch's
/// commits ahead of trunk are archived to `preserved/...`. When the stray
/// branch carries the same task id (e.g. `other-member/28` → `me/28`) and
/// `expected_branch` does not exist yet, the expected branch is created from
/// the stray HEAD so the work carries over. Returns the archive branch name
/// when one was created. Fails (without touching branches) if dirty work
/// cannot be preserved.
pub(crate) fn heal_stray_task_branch(
    worktree_path: &Path,
    expected_branch: &str,
    base_branch: &str,
    trunk_branch: &str,
) -> Result<Option<String>> {
    let subsystem = "dispatch/stray-branch-heal";
    let current_branch = git_current_branch(worktree_path).ok();
    if current_branch
        .as_deref()
        .is_some_and(|branch| branch == expected_branch || branch == base_branch)
    {
        return Ok(None);
    }
    abort_in_progress_git_operations(worktree_path);
    let label = current_branch.as_deref().unwrap_or("detached");
    let reason = prepare_worktree_for_reset(
        worktree_path,
        &format!("wip: auto-save before switching to {expected_branch} [{label}]"),
        Duration::from_secs(5),
        PreserveFailureMode::SkipReset,
        subsystem,
    )?;
    if !reason.reset_performed() {
        bail!(
            "worktree {} is on '{}' with dirty changes that could not be preserved; refusing to switch to '{}'",
            worktree_path.display(),
            label,
            expected_branch
        );
    }

    let archive = archive_checked_out_work(
        worktree_path,
        subsystem,
        trunk_branch,
        &format!("detached-{expected_branch}"),
    )?;

    let task_suffix = |branch: &str| {
        branch
            .rsplit_once('/')
            .map(|(_, suffix)| suffix.to_string())
    };
    let same_task = current_branch
        .as_deref()
        .and_then(task_suffix)
        .is_some_and(|suffix| Some(suffix) == task_suffix(expected_branch));
    if same_task && !ref_exists(worktree_path, &format!("refs/heads/{expected_branch}"))? {
        log_mutation_audit(worktree_path, subsystem, "git checkout -b", &[]);
        crate::git::cmd(worktree_path)
            .args(["checkout", "-b", expected_branch])
            .run()
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to create '{}' from stray branch '{}' in {}: {}",
                    expected_branch,
                    label,
                    worktree_path.display(),
                    error.detail()
                )
            })?;
    }
    info!(
        worktree = %worktree_path.display(),
        stray_branch = label,
        expected_branch,
        archive = archive.as_deref().unwrap_or("-"),
        carried_over = same_task,
        "healed worktree checked out on stray branch"
    );
    Ok(archive)
}

/// Iterate sub-repos (or the single repo at `worktree_path`) and archive any
/// commits on `base_branch` that are ahead of each repo's default branch
/// before a destructive `checkout -B`. Any archive failure is returned so the
/// caller aborts the reset instead of rewriting a branch it could not save.
fn archive_branch_if_commits_ahead_across_repos(
    worktree_path: &Path,
    base_branch: &str,
    trunk_branch: &str,
    subsystem: &str,
) -> Result<()> {
    for repo in repos_for_mutation(worktree_path) {
        let base_ref = effective_trunk_branch_for_repo(&repo, trunk_branch);
        archive_branch_if_commits_ahead(&repo, base_branch, &base_ref, subsystem).with_context(
            || {
                format!(
                    "aborting reset of '{}' in {}: could not archive its commits",
                    base_branch,
                    repo.display()
                )
            },
        )?;
    }
    Ok(())
}

/// Run `git checkout -B <base_branch> <default>` in each sub-repo (or the
/// single repo at `worktree_path`). A multi-repo container with no sub-repos
/// is a no-op.
pub(crate) fn checkout_base_branch_across_repos(
    worktree_path: &Path,
    base_branch: &str,
    trunk_branch: &str,
    subsystem: &str,
) -> Result<()> {
    let dirty = dirty_paths(worktree_path, ExcludePolicy::BattyRuntime).unwrap_or_default();
    let repos = repos_for_mutation(worktree_path);
    if repos.is_empty() {
        info!(
            subsystem,
            worktree = %worktree_path.display(),
            "skipping checkout -B: no git repos found at worktree root or in immediate sub-dirs"
        );
        return Ok(());
    }
    for repo in repos {
        let start = effective_trunk_branch_for_repo(&repo, trunk_branch);
        log_mutation_audit(&repo, subsystem, "git checkout -B", &dirty);
        crate::git::cmd(&repo)
            .args(["checkout", "-B", base_branch, &start])
            .run()
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to recreate '{}' from '{}' in {}: {}",
                    base_branch,
                    start,
                    repo.display(),
                    error.detail()
                )
            })?;
    }
    Ok(())
}

/// The configured trunk, or the repo's own default branch when the trunk is
/// the generic `main`.
pub(crate) fn effective_trunk_branch_for_repo(repo: &Path, trunk_branch: &str) -> String {
    if trunk_branch == "main" {
        git_cmd::default_branch_name(repo).unwrap_or_else(|| "main".to_string())
    } else {
        trunk_branch.to_string()
    }
}

/// `[worktree_path]` for a single-repo worktree, or the immediate git
/// sub-repos of a multi-repo container (possibly empty).
pub(crate) fn repos_for_mutation(worktree_path: &Path) -> Vec<PathBuf> {
    if git_cmd::is_git_repo(worktree_path) {
        vec![worktree_path.to_path_buf()]
    } else {
        git_cmd::discover_sub_repos(worktree_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_naming_helpers_agree() {
        assert_eq!(engineer_base_branch_name("eng-1"), "eng-main/eng-1");
        assert_eq!(engineer_task_branch_name("eng-1", 42), "eng-1/42");
        assert!(is_engineer_base_branch("eng-main/eng-1"));
        assert!(!is_engineer_base_branch("eng-1/42"));
        assert!(branch_belongs_to_engineer("eng-1/42", "eng-1"));
        assert!(branch_belongs_to_engineer("eng-main/eng-1", "eng-1"));
        assert!(branch_belongs_to_engineer("eng-1", "eng-1"));
        assert!(!branch_belongs_to_engineer("eng-10/42", "eng-1"));
        assert!(!branch_belongs_to_engineer("eng-main/eng-10", "eng-1"));
    }

    #[test]
    fn archive_branch_names_are_recognised_across_generations() {
        assert!(is_archive_branch("preserved/eng-1-42-1700000000"));
        assert!(is_archive_branch("legacy/eng-1-abc123"));
        assert!(is_archive_branch("backup/eng-1"));
        assert!(!is_archive_branch("eng-1/42"));
    }

    #[test]
    fn status_args_exclude_runtime_dirs_only_for_batty_policy() {
        assert_eq!(
            status_args(ExcludePolicy::Nothing),
            ["status", "--porcelain=v1", "--untracked-files=all"]
        );
        let batty = status_args(ExcludePolicy::BattyRuntime);
        assert!(batty.contains(&":(exclude).batty".to_string()));
        assert!(batty.contains(&":(exclude).cargo".to_string()));
        assert!(batty.contains(&":(exclude).batty-target".to_string()));
    }

    fn repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "ops@example.com"],
            &["config", "user.name", "Ops"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
        ] {
            crate::git::cmd(dir).args(args).run().unwrap();
        }
        tmp
    }

    #[test]
    fn dirty_paths_respects_exclude_policy() {
        let tmp = repo();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        std::fs::write(tmp.path().join(".batty/state.json"), "{}").unwrap();
        assert!(
            dirty_paths(tmp.path(), ExcludePolicy::BattyRuntime)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            dirty_paths(tmp.path(), ExcludePolicy::Nothing).unwrap(),
            [".batty/state.json"]
        );
        std::fs::write(tmp.path().join("user.txt"), "x").unwrap();
        assert_eq!(
            dirty_paths(tmp.path(), ExcludePolicy::BattyRuntime).unwrap(),
            ["user.txt"]
        );
    }

    #[test]
    fn ahead_count_and_archive_branch_with_collision_suffix() {
        let tmp = repo();
        let dir = tmp.path();
        crate::git::cmd(dir)
            .args(["checkout", "-q", "-b", "eng-1/7"])
            .run()
            .unwrap();
        crate::git::cmd(dir)
            .args(["commit", "-q", "--allow-empty", "-m", "work"])
            .run()
            .unwrap();
        assert_eq!(ahead_count(dir, "main", "eng-1/7").unwrap(), 1);
        assert_eq!(ahead_count(dir, "eng-1/7", "main").unwrap(), 0);
        assert!(ahead_count(dir, "no-such-ref", "HEAD").is_err());

        let first = archive_branch(dir, "eng-1/7", "eng-1/7").unwrap();
        let second = archive_branch(dir, "eng-1/7", "eng-1/7").unwrap();
        assert!(first.starts_with("preserved/eng-1-7-"), "{first}");
        assert_ne!(first, second);
        assert!(ref_exists(dir, &format!("refs/heads/{second}")).unwrap());
        assert!(!ref_exists(dir, "refs/heads/nope").unwrap());
    }

    fn run(dir: &Path, args: &[&str]) {
        crate::git::cmd(dir).args(args).run().unwrap();
    }

    /// A container holding one sub-repo whose default branch is `mainline`
    /// (no `main`), with `feature` checked out one commit ahead of it.
    fn mainline_container() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("PkgA");
        std::fs::create_dir_all(&sub).unwrap();
        for args in [
            &["init", "-q", "-b", "mainline"][..],
            &["config", "user.email", "ops@example.com"],
            &["config", "user.name", "Ops"],
            &["commit", "-q", "--allow-empty", "-m", "init"],
            &["checkout", "-q", "-b", "feature"],
            &["commit", "-q", "--allow-empty", "-m", "work"],
        ] {
            run(&sub, args);
        }
        (tmp, sub)
    }

    fn archives(repo: &Path) -> Vec<String> {
        crate::git::cmd(repo)
            .args([
                "branch",
                "--list",
                "--format=%(refname:short)",
                "preserved/*",
            ])
            .stdout()
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn archive_uses_per_repo_trunk_for_mainline_sub_repo() {
        let (container, sub) = mainline_container();
        // The configured trunk is the generic `main`, which this sub-repo lacks.
        let archive = archive_checked_out_work(container.path(), "test", "main", "detached")
            .unwrap()
            .expect("feature is ahead of mainline");
        assert!(archive.starts_with("preserved/feature-"), "{archive}");
        assert_eq!(archives(&sub), [archive]);
    }

    #[test]
    fn archive_detached_head_in_sub_repo_is_archived_with_detached_label() {
        let (container, sub) = mainline_container();
        run(&sub, &["checkout", "-q", "--detach"]);
        let archive = archive_checked_out_work(container.path(), "test", "main", "detached-x")
            .unwrap()
            .expect("detached HEAD is ahead of mainline");
        assert!(archive.starts_with("preserved/detached-x-"), "{archive}");
    }

    #[test]
    fn archive_with_missing_trunk_archives_unconditionally_instead_of_failing() {
        let (_container, sub) = mainline_container();
        // `develop` does not exist: nothing to compare against.
        let archive = archive_checked_out_work(&sub, "test", "develop", "detached")
            .unwrap()
            .expect("branch archived unconditionally");
        assert!(archive.starts_with("preserved/feature-"), "{archive}");

        run(&sub, &["checkout", "-q", "--detach"]);
        let detached = archive_checked_out_work(&sub, "test", "develop", "detached")
            .unwrap()
            .expect("detached HEAD archived unconditionally");
        assert!(detached.starts_with("preserved/detached-"), "{detached}");
    }

    #[test]
    fn missing_trunk_recovery_retries_reuse_the_identical_archive() {
        let (_container, sub) = mainline_container();
        let first = archive_checked_out_work(&sub, "test", "develop", "detached")
            .unwrap()
            .expect("archived");
        let second = archive_checked_out_work(&sub, "test", "develop", "detached")
            .unwrap()
            .expect("archive reused");
        assert_eq!(first, second);
        assert_eq!(archives(&sub), [first]);

        // New work on the branch is a different commit: a new archive.
        run(&sub, &["commit", "-q", "--allow-empty", "-m", "more"]);
        let third = archive_checked_out_work(&sub, "test", "develop", "detached")
            .unwrap()
            .expect("archived");
        assert_ne!(third, second);
        assert_eq!(archives(&sub).len(), 2);

        // Detached HEAD: same reuse.
        run(&sub, &["checkout", "-q", "--detach"]);
        let d1 = archive_checked_out_work(&sub, "test", "develop", "detached").unwrap();
        let d2 = archive_checked_out_work(&sub, "test", "develop", "detached").unwrap();
        assert_eq!(d1, d2);
        assert_eq!(archives(&sub).len(), 3);
    }

    #[test]
    fn archive_skips_branch_with_nothing_ahead_of_trunk() {
        let tmp = repo();
        assert_eq!(
            archive_checked_out_work(tmp.path(), "test", "main", "detached").unwrap(),
            None
        );
        run(tmp.path(), &["checkout", "-q", "--detach"]);
        assert_eq!(
            archive_checked_out_work(tmp.path(), "test", "main", "detached").unwrap(),
            None
        );
    }

    #[test]
    fn recovery_reset_succeeds_in_mainline_sub_repo_and_archives_the_branch() {
        let (container, sub) = mainline_container();
        let reason = crate::worktree::recover_worktree_to_base_preserving(
            container.path(),
            "eng-main/eng-1",
            "test",
            "main",
        )
        .unwrap();
        assert!(reason.reset_performed(), "{reason:?}");
        assert_eq!(
            crate::git::cmd(&sub)
                .args(["branch", "--show-current"])
                .stdout()
                .unwrap(),
            "eng-main/eng-1"
        );
        assert_eq!(archives(&sub).len(), 1, "{:?}", archives(&sub));
    }

    #[test]
    fn recovery_reset_succeeds_for_detached_sub_repo() {
        let (container, sub) = mainline_container();
        run(&sub, &["checkout", "-q", "--detach"]);
        let reason = crate::worktree::recover_worktree_to_base_preserving(
            container.path(),
            "eng-main/eng-1",
            "test",
            "main",
        )
        .unwrap();
        assert!(reason.reset_performed(), "{reason:?}");
        assert!(
            archives(&sub).iter().any(|name| name.contains("detached")),
            "{:?}",
            archives(&sub)
        );
    }
}
