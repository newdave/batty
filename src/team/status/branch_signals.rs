use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::warn;

use crate::task;

use super::super::config::RoleType;
use super::super::hierarchy::MemberInstance;
use super::*;

fn task_has_active_claim(task: &task::Task, member_name: &str) -> bool {
    task.claimed_by.as_deref() == Some(member_name)
        && classify_owned_task_status(task.status.as_str()) == Some(true)
}

fn managed_task_branch(member_name: &str, task: &task::Task) -> String {
    task.branch
        .clone()
        .unwrap_or_else(|| crate::worktree::ops::engineer_task_branch_name(member_name, task.id))
}

fn format_branch_mismatch_signal(
    task_id: u32,
    current_branch: &str,
    expected_branch: &str,
) -> String {
    if current_branch == "HEAD" {
        format!(
            "branch mismatch (#{} detached HEAD; expected {})",
            task_id, expected_branch
        )
    } else {
        format!(
            "branch mismatch (#{} on {}; expected {})",
            task_id, current_branch, expected_branch
        )
    }
}

fn format_blocked_branch_recovery_signal(
    task_id: u32,
    current_branch: &str,
    expected_branch: &str,
    detail: &str,
) -> String {
    if current_branch == "HEAD" {
        format!(
            "branch recovery blocked (#{} detached HEAD; expected {}; {})",
            task_id, expected_branch, detail
        )
    } else {
        format!(
            "branch recovery blocked (#{} on {}; expected {}; {})",
            task_id, current_branch, expected_branch, detail
        )
    }
}

fn select_authoritative_claimed_task<'a>(
    member_name: &str,
    current_branch: &str,
    claimed_tasks: &[&'a task::Task],
) -> Option<&'a task::Task> {
    let mut branch_matches = claimed_tasks
        .iter()
        .copied()
        .filter(|task| managed_task_branch(member_name, task) == current_branch);
    if let Some(task) = branch_matches.next()
        && branch_matches.next().is_none()
    {
        return Some(task);
    }

    claimed_tasks.iter().copied().min_by_key(|task| task.id)
}

fn task_branch_signal_for_task(
    project_root: &Path,
    task: &task::Task,
    current_branch: &str,
) -> Option<String> {
    let member_name = task.claimed_by.as_deref()?;
    if !member_name.starts_with("eng-")
        || classify_owned_task_status(task.status.as_str()) != Some(true)
    {
        return None;
    }

    let expected_branch = managed_task_branch(member_name, task);
    if current_branch == expected_branch {
        return None;
    }

    if current_branch == "HEAD" {
        return Some(format_blocked_branch_recovery_signal(
            task.id,
            current_branch,
            &expected_branch,
            "manual checkout required",
        ));
    }

    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(member_name);
    if crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap_or(false) {
        return Some(format_blocked_branch_recovery_signal(
            task.id,
            current_branch,
            &expected_branch,
            "dirty worktree",
        ));
    }

    Some(format_branch_mismatch_signal(
        task.id,
        current_branch,
        &expected_branch,
    ))
}

fn preserved_completed_lane_signal(project_root: &Path, member_name: &str) -> Option<String> {
    let record = crate::team::checkpoint::read_preserved_lane_record(project_root, member_name)?;
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(member_name);
    if !worktree_dir.is_dir() {
        return None;
    }

    let current_branch = git_stdout_raw(&worktree_dir, ["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current_branch != crate::team::task_loop::engineer_base_branch_name(member_name) {
        return None;
    }
    if crate::team::task_loop::worktree_has_user_changes(&worktree_dir).unwrap_or(false) {
        return None;
    }

    Some(record.status_signal())
}

pub(crate) fn claimed_task_branch_signal(
    project_root: &Path,
    member_name: &str,
    claimed_tasks: &[&task::Task],
) -> Option<String> {
    let worktree_dir = project_root
        .join(".batty")
        .join("worktrees")
        .join(member_name);
    if !worktree_dir.is_dir() {
        return None;
    }

    let current_branch = git_stdout_raw(&worktree_dir, ["rev-parse", "--abbrev-ref", "HEAD"])?;
    let task = select_authoritative_claimed_task(member_name, &current_branch, claimed_tasks)?;
    task_branch_signal_for_task(project_root, task, &current_branch)
}

pub(super) fn load_board_tasks_for_status(
    board_dir: &Path,
    context: &str,
) -> Result<Vec<task::Task>> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        return Ok(Vec::new());
    }

    repair_board_frontmatter_logged(board_dir, context)?;
    task::load_tasks_from_dir(&tasks_dir)
}

/// Repair malformed task frontmatter under `board_dir`, logging what changed.
pub(crate) fn repair_board_frontmatter_logged(board_dir: &Path, context: &str) -> Result<()> {
    let repairs = crate::team::task_cmd::repair_board_frontmatter_compat(board_dir)?;
    if !repairs.is_empty() {
        let repaired_tasks = repairs
            .iter()
            .map(|repair| {
                let task_label = repair
                    .task_id
                    .map(|task_id| format!("#{task_id}"))
                    .unwrap_or_else(|| repair.path.display().to_string());
                match repair.reason.as_deref() {
                    Some(reason) => format!("{task_label} ({reason})"),
                    None => task_label,
                }
            })
            .collect::<Vec<_>>();
        warn!(
            context = context,
            repaired_count = repairs.len(),
            repaired_tasks = ?repaired_tasks,
            "repaired malformed board task frontmatter during status scan"
        );
    }
    Ok(())
}

pub(crate) fn branch_mismatch_by_member(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    let tasks = if tasks_dir.is_dir() {
        match load_board_tasks_for_status(
            tasks_dir.parent().unwrap_or(&tasks_dir),
            "branch_mismatch_by_member",
        ) {
            Ok(tasks) => tasks,
            Err(error) => {
                warn!(path = %tasks_dir.display(), error = %error, "failed to load board tasks for branch mismatch status");
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer && member.use_worktrees)
        .filter_map(|member| {
            let claimed_tasks = tasks
                .iter()
                .filter(|task| task_has_active_claim(task, &member.name))
                .collect::<Vec<_>>();
            claimed_task_branch_signal(project_root, &member.name, &claimed_tasks)
                .or_else(|| preserved_completed_lane_signal(project_root, &member.name))
                .map(|signal| (member.name.clone(), signal))
        })
        .collect()
}

pub(crate) fn worktree_staleness_by_member(
    project_root: &Path,
    members: &[MemberInstance],
) -> HashMap<String, u32> {
    let trunk_branch = crate::team::config::configured_trunk_branch(project_root);
    members
        .iter()
        .filter(|member| member.role_type == RoleType::Engineer && member.use_worktrees)
        .filter_map(|member| {
            let worktree_dir = project_root
                .join(".batty")
                .join("worktrees")
                .join(&member.name);
            if !worktree_dir.exists() {
                return None;
            }

            let mut max_count: Option<u32> = None;
            for repo in status_worktree_repo_targets(&worktree_dir) {
                match super::super::task_loop::worktree_commits_behind_trunk(
                    &repo.path,
                    &trunk_branch,
                ) {
                    Ok(count) => {
                        max_count = Some(match max_count {
                            Some(current) => current.max(count),
                            None => count,
                        });
                    }
                    Err(error) => {
                        warn!(
                            member = %member.name,
                            repo = repo.label.as_deref().unwrap_or("<root>"),
                            worktree = %repo.path.display(),
                            error = %error,
                            "failed to measure engineer worktree staleness for status"
                        );
                    }
                }
            }

            max_count.map(|count| (member.name.clone(), count))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StatusWorktreeRepoTarget {
    pub(super) label: Option<String>,
    pub(super) path: PathBuf,
}

pub(super) fn status_worktree_repo_targets(worktree_dir: &Path) -> Vec<StatusWorktreeRepoTarget> {
    if super::super::git_cmd::is_git_repo(worktree_dir) {
        return vec![StatusWorktreeRepoTarget {
            label: None,
            path: worktree_dir.to_path_buf(),
        }];
    }

    super::super::git_cmd::discover_sub_repos(worktree_dir)
        .into_iter()
        .map(|path| StatusWorktreeRepoTarget {
            label: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            path,
        })
        .collect()
}

#[derive(Default)]
pub(super) struct InferredTaskMetadata {
    pub(super) branch: Option<String>,
    pub(super) worktree_path: Option<String>,
    pub(super) commit: Option<String>,
}

pub(super) fn infer_runtime_task_metadata(
    project_root: &Path,
    task: &task::Task,
) -> InferredTaskMetadata {
    let Some(claimed_by) = task.claimed_by.as_deref() else {
        return InferredTaskMetadata::default();
    };
    if !claimed_by.starts_with("eng-") {
        return InferredTaskMetadata::default();
    }

    let worktree_path = project_root
        .join(".batty")
        .join("worktrees")
        .join(claimed_by);
    if !worktree_path.is_dir() {
        return InferredTaskMetadata::default();
    }

    InferredTaskMetadata {
        branch: git_stdout_raw(&worktree_path, ["rev-parse", "--abbrev-ref", "HEAD"]),
        worktree_path: relative_to_project_root(project_root, &worktree_path),
        commit: git_stdout(&worktree_path, ["rev-parse", "--short", "HEAD"]),
    }
}

pub(super) fn task_branch_mismatch(
    project_root: &Path,
    task: &task::Task,
    inferred: &InferredTaskMetadata,
) -> Option<String> {
    let current_branch = inferred.branch.as_deref()?;
    task_branch_signal_for_task(project_root, task, current_branch)
}

fn git_stdout_raw<const N: usize>(cwd: &Path, args: [&str; N]) -> Option<String> {
    let output = crate::git::cmd(cwd).args(args).allow_fail().run().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub(super) fn git_stdout<const N: usize>(cwd: &Path, args: [&str; N]) -> Option<String> {
    git_stdout_raw(cwd, args).filter(|value| value != "HEAD")
}

fn relative_to_project_root(project_root: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(project_root)
        .ok()
        .map(|relative| relative.display().to_string())
}
