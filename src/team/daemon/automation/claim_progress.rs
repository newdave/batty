//! Liveness and progress detection for in-progress claimed tasks.

use std::time::Duration;

use chrono::{DateTime, Utc};

use super::super::*;

/// Seconds since a task file was last modified. Used as a proxy for
/// "how long since the task's last state transition" — kanban-md edits
/// the file on every transition, so mtime tracks the freshness of the
/// current status. Returns `None` if the file is unreadable (treat as
/// "unknown age" = not recently touched).
pub(super) fn task_file_age_secs(source_path: &std::path::Path) -> Option<u64> {
    let modified = std::fs::metadata(source_path).ok()?.modified().ok()?;
    modified.elapsed().ok().map(|d| d.as_secs())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct InProgressTaskLiveness {
    pub(super) dirty_worktree: bool,
    output_growth: bool,
    recent_claim_progress: bool,
    fresh_claim_grace: bool,
    live_progress_type: Option<&'static str>,
}

pub(super) fn parse_rfc3339_utc(value: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn latest_commit_timestamp(
    work_dir: &std::path::Path,
    trunk_branch: &str,
) -> Option<DateTime<Utc>> {
    if crate::team::git_cmd::rev_list_count(work_dir, &format!("{trunk_branch}..HEAD"))
        .ok()
        .is_none_or(|count| count == 0)
    {
        return None;
    }
    let output = crate::git::cmd(work_dir)
        .args(["log", "-1", "--format=%cI"])
        .allow_fail()
        .run()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_rfc3339_utc(stdout.trim())
}

fn has_claim_progress_worktree_changes(work_dir: &std::path::Path) -> bool {
    crate::team::git_cmd::status_porcelain(work_dir)
        .ok()
        .map(|status| {
            status.lines().any(|line| {
                let path = line.get(3..).unwrap_or("").trim();
                if path.starts_with(".batty/") || path.starts_with(".cargo/") {
                    return false;
                }
                !path.starts_with(".batty/team_config/")
            })
        })
        .unwrap_or(false)
}

pub(super) fn task_claim_progress_type(
    task: &crate::task::Task,
    work_dir: &std::path::Path,
    current_output_bytes: u64,
    trunk_branch: &str,
) -> Option<&'static str> {
    let last_progress_at = task
        .last_progress_at
        .as_deref()
        .and_then(parse_rfc3339_utc)?;
    if latest_commit_timestamp(work_dir, trunk_branch).is_some_and(|ts| ts > last_progress_at) {
        return Some("commit");
    }
    if has_claim_progress_worktree_changes(work_dir) {
        return Some("dirty_files");
    }
    if current_output_bytes > task.last_output_bytes.unwrap_or(0) {
        return Some("output");
    }
    None
}

fn task_recent_claim_progress(
    task: &crate::task::Task,
    now: DateTime<Utc>,
    progress_window: Duration,
) -> bool {
    let Some(last_progress_at) = task.last_progress_at.as_deref().and_then(parse_rfc3339_utc)
    else {
        return false;
    };
    let age_secs = now.signed_duration_since(last_progress_at).num_seconds();
    age_secs >= 0 && age_secs < progress_window.as_secs() as i64
}

fn fresh_claim_immunity_secs(progress_window: Duration) -> u64 {
    std::cmp::max(60, progress_window.as_secs() / 10)
}

fn task_in_fresh_claim_grace(
    task: &crate::task::Task,
    now: DateTime<Utc>,
    progress_window: Duration,
) -> bool {
    let Some(claimed_at) = task.claimed_at.as_deref().and_then(parse_rfc3339_utc) else {
        return false;
    };
    let age_secs = now.signed_duration_since(claimed_at).num_seconds();
    age_secs >= 0 && age_secs < fresh_claim_immunity_secs(progress_window) as i64
}

impl InProgressTaskLiveness {
    pub(super) fn has_live_progress(self) -> bool {
        self.fresh_claim_grace
            || self.recent_claim_progress
            || self.output_growth
            || matches!(self.live_progress_type, Some("commit"))
    }
}

impl TeamDaemon {
    pub(super) fn in_progress_task_liveness(
        &self,
        task: &crate::task::Task,
        now: DateTime<Utc>,
        progress_window: Duration,
    ) -> Option<InProgressTaskLiveness> {
        let owner = task.claimed_by.as_deref()?;
        let work_dir = self.task_progress_work_dir(task)?;
        let current_output_bytes = self
            .shim_handles
            .get(owner)
            .map(|handle| handle.output_bytes)
            .unwrap_or(0);

        Some(InProgressTaskLiveness {
            dirty_worktree: has_claim_progress_worktree_changes(&work_dir),
            output_growth: current_output_bytes > task.last_output_bytes.unwrap_or(0),
            recent_claim_progress: task_recent_claim_progress(task, now, progress_window),
            fresh_claim_grace: task_in_fresh_claim_grace(task, now, progress_window),
            live_progress_type: task_claim_progress_type(
                task,
                &work_dir,
                current_output_bytes,
                self.config.team_config.trunk_branch(),
            ),
        })
    }

    fn task_progress_work_dir(&self, task: &crate::task::Task) -> Option<std::path::PathBuf> {
        if let Some(worktree_path) = task.worktree_path.as_deref() {
            let path = std::path::PathBuf::from(worktree_path);
            return Some(if path.is_absolute() {
                path
            } else {
                self.config.project_root.join(path)
            });
        }

        let owner = task.claimed_by.as_deref()?;
        let uses_worktrees = self
            .config
            .members
            .iter()
            .find(|member| member.name == owner)
            .is_none_or(|member| member.use_worktrees);
        Some(if uses_worktrees {
            self.worktree_dir(owner)
        } else {
            self.config.project_root.clone()
        })
    }
}
