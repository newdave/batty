//! Worktree, board rotation, archive, cron, retrospective, and daemon-binary housekeeping.

use std::time::{Duration, Instant};

use anyhow::Result;
use tracing::{debug, info, warn};

use super::super::helpers::MemberWorktreeContext;
use super::super::*;
use super::*;

fn binary_refresh_blocker(decision: &crate::team::daemon_mgmt::StaleDaemonRestartReport) -> String {
    match decision.status {
        crate::team::daemon_mgmt::StaleDaemonRestartStatus::RefusedDirty => {
            if decision.dirty_paths.is_empty() {
                "dirty main".to_string()
            } else {
                format!("dirty main ({})", decision.dirty_paths.join(", "))
            }
        }
        crate::team::daemon_mgmt::StaleDaemonRestartStatus::RefusedActiveMerge => {
            "active merge".to_string()
        }
        crate::team::daemon_mgmt::StaleDaemonRestartStatus::RefusedMissingReleaseBinary => {
            "missing release binary".to_string()
        }
        _ => "not blocked".to_string(),
    }
}

impl TeamDaemon {
    pub(in crate::team::daemon) fn member_worktree_context(
        &self,
        member_name: &str,
    ) -> Option<MemberWorktreeContext> {
        if !self.member_uses_worktrees(member_name) {
            return None;
        }
        let worktree_path = self
            .config
            .project_root
            .join(".batty")
            .join("worktrees")
            .join(member_name);
        if !worktree_path.exists() {
            return None;
        }

        let branch = crate::git::cmd(&worktree_path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .allow_fail()
            .run()
            .ok()
            .and_then(|output| {
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
            })
            .filter(|branch| !branch.is_empty());

        Some(MemberWorktreeContext {
            path: worktree_path,
            branch,
        })
    }

    /// Detect engineer worktrees still on branches that have been merged to main.
    /// For idle engineers with no active task, auto-reset to their base branch.
    pub(in crate::team::daemon) fn maybe_reconcile_stale_worktrees(&mut self) -> Result<()> {
        if !self.is_git_repo && !self.is_multi_repo {
            return Ok(());
        }
        let tasks_dir = self.board_dir().join("tasks");
        let board_tasks = if tasks_dir.exists() {
            self.board_tasks()?
        } else {
            std::sync::Arc::new(Vec::new())
        };

        let engineers: Vec<(String, bool)> = self
            .config
            .members
            .iter()
            .filter(|m| m.role_type == RoleType::Engineer && m.use_worktrees)
            .map(|m| {
                let is_idle = self.states.get(&m.name) == Some(&MemberState::Idle);
                (m.name.clone(), is_idle)
            })
            .collect();
        let trunk_branch = self.config.team_config.trunk_branch().to_string();

        for (engineer, is_idle) in engineers {
            if !is_idle {
                continue;
            }
            if self.active_tasks.contains_key(&engineer) {
                continue;
            }

            let worktree_dir = self.worktree_dir(&engineer);
            if !worktree_dir.exists() {
                continue;
            }

            let branch = match current_worktree_branch(&worktree_dir) {
                Ok(b) => b,
                Err(_) => continue,
            };

            let base_branch = engineer_base_branch_name(&engineer);
            if branch == "HEAD" {
                continue;
            }

            let authorized_tasks: Vec<&crate::task::Task> = board_tasks
                .iter()
                .filter(|task| {
                    task.claimed_by.as_deref() == Some(engineer.as_str())
                        && super::super::interventions::task_needs_owned_intervention(
                            task.status.as_str(),
                        )
                })
                .collect();

            if branch == base_branch {
                match crate::team::task_loop::refresh_engineer_worktree_from_trunk(
                    &self.config.project_root,
                    &worktree_dir,
                    &base_branch,
                    &self.config.project_root.join(".batty").join("team_config"),
                    &trunk_branch,
                ) {
                    Ok(crate::team::task_loop::WorktreeRefreshAction::Rebased)
                    | Ok(crate::team::task_loop::WorktreeRefreshAction::Reset) => {
                        info!(
                            engineer = %engineer,
                            branch = %base_branch,
                            trunk_branch = %trunk_branch,
                            "auto-refreshed engineer worktree after trunk advanced"
                        );
                        self.emit_event(TeamEvent::worktree_refreshed(
                            &engineer,
                            &format!("rebased clean base worktree onto {trunk_branch}"),
                        ));
                        self.record_orchestrator_action(format!(
                            "worktree: auto-rebased clean base worktree for {engineer} onto {trunk_branch}"
                        ));
                    }
                    Ok(crate::team::task_loop::WorktreeRefreshAction::Unchanged)
                    | Ok(crate::team::task_loop::WorktreeRefreshAction::SkippedDirty) => {}
                    Err(error) => warn!(
                        engineer = %engineer,
                        branch = %base_branch,
                        error = %error,
                        "worktree refresh after main advance failed"
                    ),
                }
                continue;
            }

            // Blocking releases the active claim, not the saved task checkout.
            // Keep it available for continuation until another task is dispatched.
            if board_tasks.iter().any(|task| {
                task.status == "blocked" && authoritative_task_branch(&engineer, task) == branch
            }) {
                continue;
            }

            let completed_task_for_branch = board_tasks.iter().find(|task| {
                task.claimed_by.as_deref() == Some(engineer.as_str())
                    && task.status == "done"
                    && authoritative_task_branch(&engineer, task) == branch
            });

            if is_managed_task_branch(&engineer, &branch)
                && authorized_tasks
                    .iter()
                    .all(|task| authoritative_task_branch(&engineer, task) != branch)
            {
                self.maybe_reset_engineer_to_safe_branch(
                    &engineer,
                    &branch,
                    &authorized_tasks,
                    completed_task_for_branch,
                    "board no longer authorizes current branch",
                );
                continue;
            }

            let merged =
                match branch_is_merged_into(&self.config.project_root, &branch, &trunk_branch) {
                    Ok(m) => m,
                    Err(_) => continue,
                };

            if !merged {
                continue;
            }

            // SAFETY: never reset a worktree that has work ahead of trunk.
            // During stop/start cycles, active_tasks can be empty momentarily
            // while the engineer still has unmerged commits.
            if let Ok(ahead) = crate::worktree::commits_ahead(&worktree_dir, &trunk_branch) {
                if ahead > 0 {
                    debug!(
                        engineer = %engineer,
                        branch = %branch,
                        ahead,
                        trunk_branch = %trunk_branch,
                        "worktree has commits ahead of trunk; skipping reconciliation"
                    );
                    continue;
                }
            }

            match reset_claimed_worktree_to_base(&worktree_dir, &base_branch, &trunk_branch) {
                Ok(reset_reason) if reset_reason.reset_performed() => {
                    info!(
                        engineer = %engineer,
                        stale_branch = %branch,
                        reset_to = %base_branch,
                        reset_reason = reset_reason.as_str(),
                        "auto-reconciled stale worktree"
                    );
                    self.emit_event(TeamEvent::worktree_reconciled(&engineer, &branch));
                    self.record_orchestrator_action(format!(
                        "worktree: auto-reconciled {engineer} from stale branch '{branch}' to '{base_branch}' ({})",
                        reset_reason.as_str()
                    ));
                }
                Ok(reset_reason) => {
                    debug!(
                        engineer = %engineer,
                        branch = %branch,
                        reset_reason = reset_reason.as_str(),
                        "skipping worktree reconciliation"
                    );
                }
                Err(error) => {
                    warn!(
                        engineer = %engineer,
                        branch = %branch,
                        error = %error,
                        "worktree reconciliation failed"
                    );
                    continue;
                }
            }
        }

        Ok(())
    }

    /// Rotate the board if enough time has passed.
    ///
    /// When using kanban-md (board/ directory), rotation is not needed — each
    /// task is an individual file. Only rotates the legacy plain kanban.md.
    pub(in crate::team::daemon) fn maybe_rotate_board(&mut self) -> Result<()> {
        // Check every 10 minutes
        if self.scheduler.last_board_rotation.elapsed() < Duration::from_secs(600) {
            return Ok(());
        }

        self.scheduler.last_board_rotation = Instant::now();

        let config_dir = self.config.project_root.join(".batty").join("team_config");

        // kanban-md uses a board/ directory — no rotation needed
        let board_dir = config_dir.join("board");
        if board_dir.is_dir() {
            return Ok(());
        }

        // Legacy plain kanban.md — rotate done items
        let kanban_path = config_dir.join("kanban.md");
        let archive_path = config_dir.join("kanban-archive.md");

        if kanban_path.exists() {
            match board::rotate_done_items(
                &kanban_path,
                &archive_path,
                self.config.team_config.board.rotation_threshold,
            ) {
                Ok(rotated) if rotated > 0 => {
                    info!(rotated, "board rotation completed");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "board rotation failed");
                }
            }
        }

        Ok(())
    }

    /// Periodically archive done tasks that exceed the configured age threshold.
    ///
    /// Rate-limited to run at most once per 60 seconds. Disabled when
    /// `auto_archive_done_after_secs` is `None` or `0`.
    pub(in crate::team::daemon) fn maybe_auto_archive(&mut self) -> Result<()> {
        // Rate-limit to once per minute
        if self.scheduler.last_auto_archive.elapsed() < Duration::from_secs(60) {
            return Ok(());
        }
        self.scheduler.last_auto_archive = Instant::now();

        let threshold_secs = match self
            .config
            .team_config
            .workflow_policy
            .auto_archive_done_after_secs
        {
            Some(0) | None => return Ok(()),
            Some(secs) => secs,
        };

        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.is_dir() {
            return Ok(());
        }

        let max_age = Duration::from_secs(threshold_secs);
        let old_done = board::done_tasks_older_than(&board_dir, max_age)?;
        if old_done.is_empty() {
            return Ok(());
        }

        let summary = board::archive_tasks(&board_dir, &old_done, false)?;
        if summary.archived_count > 0 {
            for task in &old_done {
                self.record_board_task_archived(task.id, task.claimed_by.as_deref());
            }
            info!(
                archived = summary.archived_count,
                threshold_secs, "auto-archived done tasks"
            );
            self.record_orchestrator_action(format!(
                "auto-archive: archived {} done tasks older than {}s",
                summary.archived_count, threshold_secs
            ));
        }

        Ok(())
    }

    pub(in crate::team::daemon) fn maybe_recycle_cron_tasks(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks = self.board_tasks()?;
        let recycled = super::super::super::task_loop::recycle_cron_tasks_from(&board_dir, &tasks)?;
        for (task_id, cron_expr) in recycled {
            self.emit_event(TeamEvent::task_recycled(task_id, &cron_expr));
            self.record_orchestrator_action(format!(
                "cron: recycled task #{task_id} (schedule: {cron_expr}) back to todo"
            ));
        }
        Ok(())
    }

    pub(in crate::team::daemon) fn maybe_generate_retrospective(&mut self) -> Result<()> {
        if self.scheduler.retro_generated || !self.board_dir().join("tasks").is_dir() {
            return Ok(());
        }
        let tasks = self.board_tasks()?;
        let Some(stats) = super::super::super::retrospective::should_generate_retro_with_tasks(
            &self.config.project_root,
            &tasks,
            self.config.team_config.retro_min_duration_secs,
        )?
        else {
            return Ok(());
        };

        let report_path = super::super::super::retrospective::generate_retrospective(
            &self.config.project_root,
            &stats,
        )?;
        self.scheduler.retro_generated = true;
        self.record_retro_generated();
        info!(path = %report_path.display(), "retrospective generated");
        Ok(())
    }

    pub(in crate::team::daemon) fn maybe_refresh_stale_daemon_binary(&mut self) -> Result<()> {
        let Some(state) = crate::team::daemon::health::binary_freshness::load_binary_refresh_state(
            &self.config.project_root,
        )?
        else {
            return Ok(());
        };
        if state.phase
            == crate::team::daemon::health::binary_freshness::DaemonBinaryRefreshPhase::Scheduled
        {
            return Ok(());
        }

        let decision =
            crate::team::daemon_mgmt::safe_daemon_restart_decision(&self.config.project_root)?;
        let Some(freshness) = decision.freshness.as_ref() else {
            return Ok(());
        };

        match decision.status {
            crate::team::daemon_mgmt::StaleDaemonRestartStatus::DryRunReady => {
                let scheduled =
                    crate::team::daemon::health::binary_freshness::DaemonBinaryRefreshState::scheduled(
                        freshness,
                        crate::team::now_unix(),
                    );
                crate::team::daemon::health::binary_freshness::save_binary_refresh_state(
                    &self.config.project_root,
                    &scheduled,
                )?;
                self.record_orchestrator_action(format!(
                    "runtime: daemon binary restart scheduled ({} commit(s) behind main)",
                    freshness.commits_behind
                ));
                if let Err(error) = crate::team::daemon_mgmt::rebuild_reinstall_and_resign_batty(
                    &self.config.project_root,
                ) {
                    let blocked =
                        crate::team::daemon::health::binary_freshness::DaemonBinaryRefreshState::blocked(
                            freshness,
                            crate::team::now_unix(),
                            format!("restart command failed: {error}"),
                        );
                    crate::team::daemon::health::binary_freshness::save_binary_refresh_state(
                        &self.config.project_root,
                        &blocked,
                    )?;
                    return Err(error);
                }
            }
            crate::team::daemon_mgmt::StaleDaemonRestartStatus::FreshNoop => {
                crate::team::daemon::health::binary_freshness::clear_binary_refresh_state(
                    &self.config.project_root,
                )?;
            }
            crate::team::daemon_mgmt::StaleDaemonRestartStatus::RefusedDirty
            | crate::team::daemon_mgmt::StaleDaemonRestartStatus::RefusedActiveMerge
            | crate::team::daemon_mgmt::StaleDaemonRestartStatus::RefusedMissingReleaseBinary => {
                let blocked =
                    crate::team::daemon::health::binary_freshness::DaemonBinaryRefreshState::blocked(
                        freshness,
                        crate::team::now_unix(),
                        binary_refresh_blocker(&decision),
                    );
                crate::team::daemon::health::binary_freshness::save_binary_refresh_state(
                    &self.config.project_root,
                    &blocked,
                )?;
                self.record_orchestrator_action(format!(
                    "runtime: daemon binary restart blocked: {}",
                    binary_refresh_blocker(&decision)
                ));
            }
            crate::team::daemon_mgmt::StaleDaemonRestartStatus::Restarted => {}
        }

        Ok(())
    }
}
