//! Topology reconciliation: apply a TopologyDiff to the running daemon state.
//!
//! When team.yaml changes and the daemon detects a topology diff, this module
//! spawns new agents for added members and gracefully shuts down removed ones.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use super::TeamDaemon;
use super::deferred::PendingRemoval;
use super::helpers::{kill_pid, reap_if_exited, signalable_pid};
use crate::shim::protocol::ShimState;
use crate::task::load_tasks_from_dir;
use crate::team::config::{RoleType, TeamConfig};
use crate::team::config_diff::TopologyDiff;
use crate::team::events::TeamEvent;
use crate::team::git_cmd;
use crate::team::hierarchy::MemberInstance;
use crate::team::inbox;
use crate::team::standup::MemberState;
use crate::team::task_cmd;
use crate::team::task_loop::{
    branch_is_merged_into, current_worktree_branch, engineer_base_branch_name,
};
use crate::team::watcher::SessionWatcher;
use crate::tmux;

impl TeamDaemon {
    /// Apply a topology diff to the running daemon: spawn new agents, remove old ones,
    /// and update internal state.
    pub(super) fn reconcile_topology(
        &mut self,
        diff: TopologyDiff,
        new_config: TeamConfig,
        new_members: Vec<MemberInstance>,
    ) -> Result<()> {
        // Phase 1: Gracefully remove agents that are no longer in the topology
        for change in &diff.removed {
            if change.member.role_type == RoleType::User {
                continue;
            }
            self.remove_member(&change.name)?;
        }

        // Phase 2: Create tmux panes and spawn agents for added members
        for change in &diff.added {
            if change.member.role_type == RoleType::User {
                continue;
            }
            if let Err(e) = self.add_member(&change.member) {
                warn!(
                    member = change.name.as_str(),
                    error = %e,
                    "failed to add member during reconciliation"
                );
            }
        }

        // Phase 3: Update daemon config to reflect new topology
        self.config.team_config = new_config;
        self.config.members = new_members;

        info!(
            added = diff.added.len(),
            removed = diff.removed.len(),
            total_members = self.config.members.len(),
            "topology reconciliation complete"
        );

        Ok(())
    }

    /// Add a new member: create tmux pane, set up worktree/inbox, spawn shim.
    fn add_member(&mut self, member: &MemberInstance) -> Result<()> {
        info!(
            member = member.name.as_str(),
            "adding new member to topology"
        );

        // Create inbox
        let inboxes = inbox::inboxes_root(&self.config.project_root);
        if let Err(e) = inbox::init_inbox(&inboxes, &member.name) {
            warn!(member = member.name.as_str(), error = %e, "failed to init inbox");
        }

        // Create tmux pane by splitting an existing one in the session
        let pane_id = self.create_pane_for_member(member)?;
        self.config
            .pane_map
            .insert(member.name.clone(), pane_id.clone());

        // Create watcher for the new pane
        let stale_secs = self.config.team_config.standup.interval_secs * 2;
        self.watchers.insert(
            member.name.clone(),
            SessionWatcher::new(&pane_id, &member.name, stale_secs, None),
        );

        // Set initial state
        self.states.insert(member.name.clone(), MemberState::Idle);

        // Spawn shim subprocess if use_shim is enabled
        if self.config.team_config.use_shim {
            let work_dir = self.member_work_dir(member);
            let agent_name = member.agent.as_deref().unwrap_or("claude");
            // The shim --cmd expects the CLI command name (e.g. "claude", "codex")
            let agent_cmd = agent_name.to_string();

            let pty_log_path = crate::team::shim_log_path(&self.config.project_root, &member.name);
            let events_log_path =
                crate::team::shim_events_log_path(&self.config.project_root, &member.name);
            if let Some(parent) = pty_log_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = crate::team::layout::respawn_as_display_pane(
                &pane_id,
                &self.config.project_root,
                &member.name,
                &events_log_path,
                &pty_log_path,
            ) {
                warn!(
                    member = member.name.as_str(),
                    pane = pane_id.as_str(),
                    error = %error,
                    "failed to respawn topology-added member pane as console pane"
                );
            }

            let sdk_mode = super::launcher::agent_supports_sdk_mode(agent_name)
                && self.config.team_config.use_sdk_mode;
            match super::shim_spawn::spawn_shim(
                &member.name,
                agent_name,
                &agent_cmd,
                &self.config.project_root,
                &work_dir,
                Some(&pty_log_path),
                self.config
                    .team_config
                    .workflow_policy
                    .graceful_shutdown_timeout_secs,
                self.config
                    .team_config
                    .workflow_policy
                    .auto_commit_on_restart,
                sdk_mode,
            ) {
                Ok(handle) => {
                    self.shim_handles.insert(member.name.clone(), handle);
                    info!(member = member.name.as_str(), "shim spawned for new member");
                }
                Err(e) => {
                    warn!(
                        member = member.name.as_str(),
                        error = %e,
                        "failed to spawn shim for new member"
                    );
                }
            }
        }

        // Log event
        self.emit_event(TeamEvent::agent_spawned(&member.name));

        self.record_orchestrator_action(format!("topology: added member {}", member.name));
        Ok(())
    }

    /// Remove a member: graceful shim shutdown, kill tmux pane, clean up state.
    fn remove_member(&mut self, name: &str) -> Result<()> {
        info!(member = name, "removing member from topology");

        if let Err(error) = self.requeue_removed_member_tasks(name) {
            warn!(
                member = name,
                error = %error,
                "failed to requeue board tasks for removed member"
            );
        }

        // Check if agent is currently working
        let is_working = self
            .shim_handles
            .get(name)
            .is_some_and(|h| h.state == ShimState::Working);

        if is_working {
            info!(
                member = name,
                "agent is working — sending graceful shutdown with timeout"
            );
        }

        // Phase 1: Send graceful shutdown to shim handle
        let timeout_secs = self.config.team_config.shim_shutdown_timeout_secs;
        if let Some(handle) = self.shim_handles.get_mut(name) {
            if !handle.is_terminal() {
                if let Err(e) = handle.send_shutdown(timeout_secs) {
                    warn!(member = name, error = %e, "failed to send shutdown to shim");
                    let _ = handle.send_kill();
                }
            }
        }

        // Phase 2: Do not wait for the shim inside the tick. Record the
        // pending removal; `process_pending_removals` reaps the process on
        // later ticks (SIGKILL after the shutdown timeout) and then finishes.
        let live_pid = self
            .shim_handles
            .get(name)
            .filter(|handle| !handle.is_terminal())
            .map(|handle| handle.child_pid)
            .filter(|pid| signalable_pid(*pid).is_some());
        if let Some(pid) = live_pid {
            self.deferred.pending_removals.insert(
                name.to_string(),
                PendingRemoval {
                    pid,
                    deadline: Instant::now() + Duration::from_secs(timeout_secs as u64),
                    was_working: is_working,
                    killed: false,
                },
            );
            return Ok(());
        }

        self.finish_member_removal(name, is_working)
    }

    /// Advance removals started by `remove_member`: finish once the shim has
    /// exited, or force-kill it when the shutdown timeout elapses.
    pub(super) fn process_pending_removals(&mut self) -> Result<()> {
        let names: Vec<String> = self.deferred.pending_removals.keys().cloned().collect();
        for name in names {
            let Some(pending) = self.deferred.pending_removals.get(&name).cloned() else {
                continue;
            };
            // `reap_if_exited` waits on zombies, which `kill(pid, 0)` would
            // report as alive for the full timeout.
            if reap_if_exited(pending.pid) {
                debug!(
                    member = name.as_str(),
                    pid = pending.pid,
                    "shim process exited"
                );
            } else if !pending.killed && Instant::now() >= pending.deadline {
                warn!(
                    member = name.as_str(),
                    pid = pending.pid,
                    "shim did not exit in time — sending SIGKILL"
                );
                kill_pid(pending.pid, libc::SIGKILL);
                if let Some(entry) = self.deferred.pending_removals.get_mut(&name) {
                    entry.killed = true;
                }
                continue;
            } else {
                continue;
            }
            self.deferred.pending_removals.remove(&name);
            if let Err(error) = self.finish_member_removal(&name, pending.was_working) {
                warn!(member = name.as_str(), error = %error, "failed to finish member removal");
            }
        }
        Ok(())
    }

    fn finish_member_removal(&mut self, name: &str, is_working: bool) -> Result<()> {
        // Phase 3: Remove shim handle
        self.shim_handles.remove(name);

        // Phase 4: Kill tmux pane
        if let Some(pane_id) = self.config.pane_map.remove(name) {
            if let Err(e) = tmux::kill_pane(&pane_id) {
                debug!(
                    member = name,
                    error = %e,
                    "failed to kill pane (may already be gone)"
                );
            }
        }

        // Phase 5: Clean up daemon state maps
        self.watchers.remove(name);
        self.states.remove(name);
        self.health.working_since.remove(name);
        self.idle_started_at.remove(name);
        self.active_tasks.remove(name);
        self.retry_counts.remove(name);
        self.interventions.triage_idle_epochs.remove(name);
        self.interventions.triage_interventions.remove(name);
        self.interventions.owned_task_interventions.remove(name);
        self.scheduler.intervention_cooldowns.remove(name);
        self.interventions.nudges.remove(name);
        self.scheduler.last_standup.remove(name);
        self.health.backend.remove(name);
        self.health.last_uncommitted_warn.remove(name);
        self.delivery.pending_queue.remove(name);

        if let Err(error) = self.cleanup_removed_member_worktree(name) {
            warn!(
                member = name,
                error = %error,
                "failed to clean removed member worktree"
            );
        }

        // Log event
        let reason = if is_working {
            "removed after graceful shutdown (was working)"
        } else {
            "removed (was idle)"
        };
        self.emit_event(TeamEvent::agent_removed(name, reason));

        self.record_orchestrator_action(format!("topology: removed member {name}"));
        Ok(())
    }

    pub(super) fn cleanup_removed_member_worktree(&mut self, name: &str) -> Result<()> {
        let project_root = self.config.project_root.clone();
        let worktree_dir = self.worktree_dir(name);
        if !self.member_uses_worktrees(name) && !worktree_dir.exists() {
            return Ok(());
        }
        // Members removed while the daemon was down never went through
        // remove_member; release their active claims so cleanup can proceed.
        if let Err(error) = self.requeue_removed_member_tasks(name) {
            warn!(member = name, error = %error, "failed to requeue board tasks for removed member");
        }
        if self.removed_member_still_owns_board_work(name)? {
            self.warn_removed_member_cleanup_refusal(
                "board-claims-work",
                name,
                &worktree_dir,
                None,
                "skipping removed member worktree cleanup because board still claims work for member",
            );
            return Ok(());
        }

        if self.is_multi_repo {
            for repo_name in self.sub_repo_names.clone() {
                let repo_root = project_root.join(&repo_name);
                let sub_worktree = worktree_dir.join(&repo_name);
                self.cleanup_removed_member_git_worktree(name, &repo_root, &sub_worktree)?;
            }
            if worktree_dir
                .read_dir()
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false)
            {
                std::fs::remove_dir(&worktree_dir)?;
            }
        } else {
            self.cleanup_removed_member_git_worktree(name, &project_root, &worktree_dir)?;
        }
        Ok(())
    }

    pub(super) fn cleanup_unconfigured_member_worktrees(&mut self) -> Result<()> {
        let worktrees_root = self.config.project_root.join(".batty").join("worktrees");
        if !worktrees_root.exists() {
            return Ok(());
        }
        let configured_members: BTreeSet<String> = self
            .config
            .members
            .iter()
            .map(|member| member.name.clone())
            .collect();

        for entry in std::fs::read_dir(&worktrees_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if configured_members.contains(&name) {
                continue;
            }
            let path = entry.path();
            if !self.is_multi_repo && !path.join(".git").exists() {
                continue;
            }
            self.cleanup_removed_member_worktree(&name)?;
        }
        Ok(())
    }

    fn removed_member_still_owns_board_work(&self, name: &str) -> Result<bool> {
        let tasks_dir = self.board_dir().join("tasks");
        if !tasks_dir.exists() {
            return Ok(false);
        }
        Ok(load_tasks_from_dir(&tasks_dir)?.into_iter().any(|task| {
            task.claimed_by.as_deref() == Some(name) && !task_is_finished(&task.status)
        }))
    }

    fn cleanup_removed_member_git_worktree(
        &mut self,
        name: &str,
        repo_root: &Path,
        worktree_dir: &Path,
    ) -> Result<()> {
        let mut branches = removed_member_branch_candidates(repo_root, name)?;

        if worktree_dir.exists() {
            let current_branch = current_worktree_branch(worktree_dir)?;
            if !removed_member_owns_worktree_branch(name, &current_branch) {
                self.warn_removed_member_cleanup_refusal(
                    "outside-namespace",
                    name,
                    worktree_dir,
                    Some(&current_branch),
                    "skipping removed member worktree cleanup because branch is outside member namespace",
                );
                return Ok(());
            }
            if crate::team::task_loop::worktree_has_user_changes(worktree_dir)? {
                self.warn_removed_member_cleanup_refusal(
                    "dirty-worktree",
                    name,
                    worktree_dir,
                    Some(&current_branch),
                    "skipping removed member worktree cleanup because worktree has uncommitted changes; commit, stash, or clear the worktree before cleanup",
                );
                return Ok(());
            }
            branches.insert(current_branch);

            git_cmd::worktree_remove(repo_root, worktree_dir, true).map_err(|error| {
                anyhow::anyhow!(
                    "failed to remove worktree '{}': {error}",
                    worktree_dir.display()
                )
            })?;
            info!(
                member = name,
                worktree = %worktree_dir.display(),
                "removed worktree for topology-removed member"
            );
        }

        for branch in branches {
            cleanup_removed_member_branch(
                repo_root,
                name,
                &branch,
                self.config.team_config.trunk_branch(),
            )?;
        }

        self.record_orchestrator_action(format!(
            "topology: cleaned removed member worktree state for {name}"
        ));
        Ok(())
    }

    fn warn_removed_member_cleanup_refusal(
        &mut self,
        reason: &str,
        name: &str,
        worktree_dir: &Path,
        branch: Option<&str>,
        message: &str,
    ) {
        let key = removed_member_cleanup_refusal_key(reason, name, worktree_dir, branch);
        // Log each distinct refusal once; repeating it every cycle is noise.
        if self.scheduler.intervention_cooldowns.contains_key(&key) {
            return;
        }

        match branch {
            Some(branch) => warn!(
                member = name,
                worktree = %worktree_dir.display(),
                branch = %branch,
                "{message}"
            ),
            None => warn!(
                member = name,
                worktree = %worktree_dir.display(),
                "{message}"
            ),
        }
        self.scheduler
            .intervention_cooldowns
            .insert(key, Instant::now());
    }

    fn requeue_removed_member_tasks(&mut self, name: &str) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }

        for task in load_tasks_from_dir(&tasks_dir)? {
            if task.claimed_by.as_deref() != Some(name) || task_is_finished(&task.status) {
                continue;
            }

            if task.status == "in-progress" || task.status == "review" {
                task_cmd::transition_task_with_attribution(
                    &board_dir,
                    task.id,
                    "todo",
                    task_cmd::StatusTransitionAttribution::daemon("daemon.reconcile.topology"),
                )?;
            }
            task_cmd::unclaim_task(&board_dir, task.id)?;
            self.record_orchestrator_action(format!(
                "topology: requeued task #{} from removed member {}",
                task.id, name
            ));
        }

        Ok(())
    }

    /// Create a new tmux pane for a member by splitting an existing one.
    fn create_pane_for_member(&self, member: &MemberInstance) -> Result<String> {
        let target_pane = self.find_split_target(member);

        let pane_id = tmux::split_window_vertical_in_pane(&self.config.session, &target_pane, 50)
            .with_context(|| format!("failed to create pane for {}", member.name))?;

        // Label the pane with @batty_role
        let _ = tmux::run_tmux_with_timeout(
            ["select-pane", "-t", &pane_id, "-T", &member.name],
            "select-pane -T",
            Some(&pane_id),
        );
        let _ = tmux::run_tmux_with_timeout(
            [
                "set-option",
                "-p",
                "-t",
                &pane_id,
                "@batty_role",
                &member.name,
            ],
            "set-option @batty_role",
            Some(&pane_id),
        );

        info!(
            member = member.name.as_str(),
            pane_id = pane_id.as_str(),
            "created tmux pane"
        );
        Ok(pane_id)
    }

    /// Find the best existing pane to split when adding a new member.
    /// Prefers splitting a pane that belongs to the same role type.
    fn find_split_target(&self, member: &MemberInstance) -> String {
        // Try to find a pane with the same role type
        for existing in &self.config.members {
            if existing.role_type == member.role_type && existing.role_type != RoleType::User {
                if let Some(pane_id) = self.config.pane_map.get(&existing.name) {
                    return pane_id.clone();
                }
            }
        }
        // Fall back to any non-user pane
        for existing in &self.config.members {
            if existing.role_type != RoleType::User {
                if let Some(pane_id) = self.config.pane_map.get(&existing.name) {
                    return pane_id.clone();
                }
            }
        }
        // Last resort: first pane in the map
        self.config
            .pane_map
            .values()
            .next()
            .cloned()
            .unwrap_or_else(|| "%0".to_string())
    }
}

fn removed_member_branch_candidates(repo_root: &Path, name: &str) -> Result<BTreeSet<String>> {
    let branches = git_cmd::for_each_ref_branches(repo_root).map_err(|error| {
        anyhow::anyhow!("failed to list branches for '{name}' cleanup: {error}")
    })?;
    Ok(branches
        .into_iter()
        .filter(|branch| removed_member_owns_branch(name, branch))
        .collect())
}

fn removed_member_owns_branch(name: &str, branch: &str) -> bool {
    branch == name
        || branch == engineer_base_branch_name(name)
        || branch
            .strip_prefix(name)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn removed_member_owns_worktree_branch(name: &str, branch: &str) -> bool {
    removed_member_owns_branch(name, branch)
        || transient_removed_member_owner(name)
            .is_some_and(|owner| removed_member_owns_branch(owner, branch))
}

fn transient_removed_member_owner(name: &str) -> Option<&str> {
    if let Some((owner, task_id)) = name.rsplit_once("-task")
        && !owner.is_empty()
        && !task_id.is_empty()
        && task_id.chars().all(|ch| ch.is_ascii_digit())
    {
        return Some(owner);
    }

    let stem = name.strip_suffix("-fix")?;
    let (owner, task_id) = stem.rsplit_once('-')?;
    if owner.is_empty() || task_id.is_empty() || !task_id.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(owner)
}

fn removed_member_cleanup_refusal_key(
    reason: &str,
    name: &str,
    worktree_dir: &Path,
    branch: Option<&str>,
) -> String {
    format!(
        "removed-member-worktree-cleanup::{reason}::{name}::{}::{}",
        worktree_dir.display(),
        branch.unwrap_or("")
    )
}

fn cleanup_removed_member_branch(
    repo_root: &Path,
    name: &str,
    branch: &str,
    trunk_branch: &str,
) -> Result<()> {
    if !git_cmd::show_ref_exists(repo_root, branch).map_err(|error| {
        anyhow::anyhow!("failed to check branch '{branch}' before cleanup: {error}")
    })? {
        return Ok(());
    }

    if branch_is_merged_into(repo_root, branch, trunk_branch)? {
        git_cmd::branch_delete(repo_root, branch)
            .map_err(|error| anyhow::anyhow!("failed to delete branch '{branch}': {error}"))?;
        info!(
            member = name,
            branch = %branch,
            "deleted branch for topology-removed member"
        );
        return Ok(());
    }

    let archive_branch = archived_removed_member_branch_name(repo_root, branch)?;
    git_cmd::branch_rename(repo_root, branch, &archive_branch).map_err(|error| {
        anyhow::anyhow!("failed to archive branch '{branch}' as '{archive_branch}': {error}")
    })?;
    warn!(
        member = name,
        branch = %branch,
        archive_branch = %archive_branch,
        "archived unmerged branch for topology-removed member"
    );
    Ok(())
}

fn archived_removed_member_branch_name(repo_root: &Path, branch: &str) -> Result<String> {
    let mut candidate = format!("archived/removed-members/{branch}");
    let mut counter = 1usize;
    while git_cmd::show_ref_exists(repo_root, &candidate).map_err(|error| {
        anyhow::anyhow!("failed to check archive branch '{candidate}' before cleanup: {error}")
    })? {
        counter += 1;
        candidate = format!("archived/removed-members/{branch}-{counter}");
    }
    Ok(candidate)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Finished tasks keep their historical claim and must not block worktree cleanup.
fn task_is_finished(status: &str) -> bool {
    matches!(status, "done" | "archived")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        cleanup_removed_member_branch, removed_member_owns_worktree_branch,
        transient_removed_member_owner,
    };
    use crate::team::config::RoleType;
    use crate::team::config_diff::{MemberChange, TopologyDiff};
    use crate::team::hierarchy::MemberInstance;
    use crate::team::test_helpers::make_test_daemon;
    use crate::team::test_support::{
        engineer_member, git_ok, git_stdout, init_git_repo, write_board_task_file,
    };

    fn make_member(name: &str, role_type: RoleType) -> MemberInstance {
        MemberInstance {
            name: name.to_string(),
            role_name: match role_type {
                RoleType::Engineer => "engineer".to_string(),
                RoleType::Manager => "manager".to_string(),
                RoleType::Architect => "architect".to_string(),
                RoleType::User => "user".to_string(),
            },
            role_type,
            agent: Some("claude".to_string()),
            prompt: None,
            reports_to: None,
            use_worktrees: role_type == RoleType::Engineer,
            ..Default::default()
        }
    }

    #[test]
    fn empty_diff_is_noop() {
        let diff = TopologyDiff {
            added: vec![],
            removed: vec![],
            unchanged: vec!["architect".to_string()],
        };
        assert!(diff.is_empty());
        assert_eq!(diff.change_count(), 0);
    }

    #[test]
    fn diff_with_additions_counts_correctly() {
        let diff = TopologyDiff {
            added: vec![
                MemberChange {
                    name: "eng-1-3".to_string(),
                    member: make_member("eng-1-3", RoleType::Engineer),
                },
                MemberChange {
                    name: "eng-1-4".to_string(),
                    member: make_member("eng-1-4", RoleType::Engineer),
                },
            ],
            removed: vec![],
            unchanged: vec!["architect".to_string()],
        };
        assert!(!diff.is_empty());
        assert_eq!(diff.change_count(), 2);
    }

    #[test]
    fn diff_with_mixed_changes() {
        let diff = TopologyDiff {
            added: vec![MemberChange {
                name: "eng-1-5".to_string(),
                member: make_member("eng-1-5", RoleType::Engineer),
            }],
            removed: vec![MemberChange {
                name: "eng-1-1".to_string(),
                member: make_member("eng-1-1", RoleType::Engineer),
            }],
            unchanged: vec!["architect".to_string()],
        };
        assert!(!diff.is_empty());
        assert_eq!(diff.change_count(), 2);
    }

    #[test]
    fn find_split_target_prefers_same_role_type() {
        let member = make_member("eng-1-3", RoleType::Engineer);
        let members = vec![
            make_member("architect", RoleType::Architect),
            make_member("manager", RoleType::Manager),
            make_member("eng-1-1", RoleType::Engineer),
            make_member("eng-1-2", RoleType::Engineer),
        ];
        let mut pane_map = HashMap::new();
        pane_map.insert("architect".to_string(), "%0".to_string());
        pane_map.insert("manager".to_string(), "%1".to_string());
        pane_map.insert("eng-1-1".to_string(), "%2".to_string());
        pane_map.insert("eng-1-2".to_string(), "%3".to_string());

        // Simulate the find_split_target logic
        let mut target = None;
        for existing in &members {
            if existing.role_type == member.role_type && existing.role_type != RoleType::User {
                if let Some(pane_id) = pane_map.get(&existing.name) {
                    target = Some(pane_id.clone());
                    break;
                }
            }
        }
        assert_eq!(target, Some("%2".to_string()));
    }

    #[test]
    fn find_split_target_falls_back_to_any_non_user() {
        let member = make_member("eng-1-1", RoleType::Engineer);
        let members = vec![
            make_member("user", RoleType::User),
            make_member("architect", RoleType::Architect),
        ];
        let mut pane_map = HashMap::new();
        pane_map.insert("architect".to_string(), "%0".to_string());

        // No engineer panes exist, should fall back to architect
        let mut target = None;
        for existing in &members {
            if existing.role_type == member.role_type && existing.role_type != RoleType::User {
                if let Some(pane_id) = pane_map.get(&existing.name) {
                    target = Some(pane_id.clone());
                    break;
                }
            }
        }
        if target.is_none() {
            for existing in &members {
                if existing.role_type != RoleType::User {
                    if let Some(pane_id) = pane_map.get(&existing.name) {
                        target = Some(pane_id.clone());
                        break;
                    }
                }
            }
        }
        assert_eq!(target, Some("%0".to_string()));
    }

    #[test]
    fn remove_member_defers_wait_and_reaps_on_later_ticks() {
        use crate::shim::protocol::{Channel, socketpair};
        use crate::team::daemon::agent_handle::AgentHandle;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), Vec::new());
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (parent_sock, _child_sock) = socketpair().unwrap();
        let handle = AgentHandle::new(
            "eng-9".into(),
            Channel::new(parent_sock),
            child.id(),
            "claude".into(),
            "claude".into(),
            tmp.path().to_path_buf(),
        );
        daemon.shim_handles.insert("eng-9".into(), handle);

        // Returns immediately even though the shim ignores the shutdown.
        let started = Instant::now();
        daemon.remove_member("eng-9").unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(daemon.shim_handles.contains_key("eng-9"));
        assert!(daemon.deferred.pending_removals.contains_key("eng-9"));

        // Before the deadline nothing happens.
        daemon.process_pending_removals().unwrap();
        assert!(daemon.shim_handles.contains_key("eng-9"));
        assert!(!daemon.deferred.pending_removals["eng-9"].killed);

        // After the deadline the shim is killed, then reaped and removed.
        daemon
            .deferred
            .pending_removals
            .get_mut("eng-9")
            .unwrap()
            .deadline = Instant::now();
        daemon.process_pending_removals().unwrap();
        assert!(daemon.deferred.pending_removals["eng-9"].killed);
        let deadline = Instant::now() + Duration::from_secs(5);
        while daemon.deferred.pending_removals.contains_key("eng-9") {
            assert!(Instant::now() < deadline, "shim never reaped");
            std::thread::sleep(Duration::from_millis(20));
            daemon.process_pending_removals().unwrap();
        }
        assert!(!daemon.shim_handles.contains_key("eng-9"));
        // Already reaped by the daemon.
        let _ = child.try_wait();
    }

    #[test]
    fn remove_member_without_live_shim_finishes_immediately() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), Vec::new());
        daemon
            .states
            .insert("eng-9".into(), crate::team::standup::MemberState::Idle);
        daemon.remove_member("eng-9").unwrap();
        assert!(daemon.deferred.pending_removals.is_empty());
        assert!(!daemon.states.contains_key("eng-9"));
    }

    #[test]
    fn requeue_removed_member_tasks_moves_in_progress_work_back_to_todo() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(
            tmp.path(),
            vec![engineer_member("eng-1", Some("manager"), false)],
        );
        write_board_task_file(
            tmp.path(),
            42,
            "active-task",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.requeue_removed_member_tasks("eng-1").unwrap();

        let task = crate::task::load_tasks_from_dir(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap()
        .into_iter()
        .find(|task| task.id == 42)
        .unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(task.claimed_by, None);
    }

    #[test]
    fn requeue_removed_member_tasks_unclaims_todo_work_without_changing_status() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(
            tmp.path(),
            vec![engineer_member("eng-1", Some("manager"), false)],
        );
        write_board_task_file(
            tmp.path(),
            43,
            "queued-task",
            "todo",
            Some("eng-1"),
            &[],
            None,
        );

        daemon.requeue_removed_member_tasks("eng-1").unwrap();

        let task = crate::task::load_tasks_from_dir(
            &tmp.path()
                .join(".batty")
                .join("team_config")
                .join("board")
                .join("tasks"),
        )
        .unwrap()
        .into_iter()
        .find(|task| task.id == 43)
        .unwrap();
        assert_eq!(task.status, "todo");
        assert_eq!(task.claimed_by, None);
    }

    #[test]
    fn removed_member_finished_claims_do_not_block_cleanup_or_get_requeued() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(
            tmp.path(),
            vec![engineer_member("eng-1", Some("manager"), false)],
        );
        write_board_task_file(tmp.path(), 50, "old", "done", Some("eng-1"), &[], None);
        assert!(
            !daemon
                .removed_member_still_owns_board_work("eng-1")
                .unwrap()
        );
        daemon.requeue_removed_member_tasks("eng-1").unwrap();
        assert!(
            !daemon
                .removed_member_still_owns_board_work("eng-1")
                .unwrap()
        );
        write_board_task_file(
            tmp.path(),
            51,
            "live",
            "in-progress",
            Some("eng-1"),
            &[],
            None,
        );
        assert!(
            daemon
                .removed_member_still_owns_board_work("eng-1")
                .unwrap()
        );
    }

    #[test]
    fn removed_member_cleanup_refusal_is_logged_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut daemon = make_test_daemon(tmp.path(), vec![]);
        let dir = tmp.path().join("wt");
        daemon.warn_removed_member_cleanup_refusal("r", "eng-1", &dir, None, "m");
        let key = super::removed_member_cleanup_refusal_key("r", "eng-1", &dir, None);
        let first = daemon.scheduler.intervention_cooldowns[&key];
        std::thread::sleep(std::time::Duration::from_millis(5));
        daemon.warn_removed_member_cleanup_refusal("r", "eng-1", &dir, None, "m");
        assert_eq!(daemon.scheduler.intervention_cooldowns[&key], first);
    }

    #[test]
    fn transient_removed_member_owner_parses_task_and_fix_worktree_names() {
        assert_eq!(
            transient_removed_member_owner("eng-1-3-task718"),
            Some("eng-1-3")
        );
        assert_eq!(
            transient_removed_member_owner("eng-1-3-726-fix"),
            Some("eng-1-3")
        );
        assert_eq!(transient_removed_member_owner("eng-1-3"), None);
        assert_eq!(transient_removed_member_owner("eng-1-3-task"), None);
        assert_eq!(transient_removed_member_owner("eng-1-3-fixabc"), None);
    }

    #[test]
    fn removed_member_owns_worktree_branch_accepts_transient_owner_namespace() {
        assert!(removed_member_owns_worktree_branch(
            "eng-1-3-task718",
            "eng-1-3/718"
        ));
        assert!(removed_member_owns_worktree_branch(
            "eng-1-3-726-fix",
            "eng-1-3/726-fix"
        ));
        assert!(!removed_member_owns_worktree_branch(
            "eng-1-3-task718",
            "eng-1-4/718"
        ));
    }

    #[test]
    fn cleanup_removed_member_worktree_removes_clean_transient_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "removed-transient-clean");
        let worktree_dir = repo
            .join(".batty")
            .join("worktrees")
            .join("eng-1-3-task718");
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-3/718",
                worktree_dir.to_str().unwrap(),
                "main",
            ],
        );
        let mut daemon = make_test_daemon(&repo, vec![]);

        daemon
            .cleanup_removed_member_worktree("eng-1-3-task718")
            .unwrap();

        assert!(
            !worktree_dir.exists(),
            "clean transient worktree should be removed"
        );
        assert_eq!(
            git_stdout(&repo, &["branch", "--list", "eng-1-3/718"]),
            "",
            "merged transient branch should be deleted"
        );
    }

    #[test]
    fn cleanup_removed_member_branch_uses_configured_trunk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "removed-mainline-merged");
        git_ok(&repo, &["checkout", "-b", "mainline"]);
        git_ok(&repo, &["checkout", "-b", "eng-1/removed"]);
        std::fs::write(repo.join("src").join("removed.rs"), "pub fn removed() {}\n").unwrap();
        git_ok(&repo, &["add", "."]);
        git_ok(&repo, &["commit", "-m", "removed member branch"]);
        git_ok(&repo, &["checkout", "mainline"]);
        git_ok(
            &repo,
            &["merge", "--no-ff", "eng-1/removed", "-m", "merge removed"],
        );
        git_ok(&repo, &["checkout", "main"]);

        cleanup_removed_member_branch(&repo, "eng-1", "eng-1/removed", "mainline").unwrap();

        assert_eq!(
            git_stdout(&repo, &["branch", "--list", "eng-1/removed"]),
            "",
            "branch merged into configured trunk should be deleted"
        );
    }

    #[test]
    fn cleanup_removed_member_worktree_preserves_dirty_transient_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "removed-transient-dirty");
        let worktree_dir = repo
            .join(".batty")
            .join("worktrees")
            .join("eng-1-3-726-fix");
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-3/726-fix",
                worktree_dir.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree_dir.join("wip.txt"), "keep me\n").unwrap();
        let mut daemon = make_test_daemon(&repo, vec![]);

        daemon
            .cleanup_removed_member_worktree("eng-1-3-726-fix")
            .unwrap();

        assert!(
            worktree_dir.exists(),
            "dirty transient worktree must be preserved"
        );
        assert!(
            worktree_dir.join("wip.txt").exists(),
            "dirty worktree contents must remain in place"
        );
        assert!(
            daemon
                .scheduler
                .intervention_cooldowns
                .keys()
                .any(|key| key.starts_with("removed-member-worktree-cleanup::dirty-worktree::")),
            "dirty preservation should record a cleanup refusal cooldown"
        );
    }

    #[test]
    fn cleanup_removed_member_worktree_dedupes_repeated_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "removed-transient-dedupe");
        let worktree_dir = repo
            .join(".batty")
            .join("worktrees")
            .join("eng-1-3-task718");
        git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "eng-1-4/718",
                worktree_dir.to_str().unwrap(),
                "main",
            ],
        );
        let mut daemon = make_test_daemon(&repo, vec![]);

        daemon
            .cleanup_removed_member_worktree("eng-1-3-task718")
            .unwrap();
        daemon
            .cleanup_removed_member_worktree("eng-1-3-task718")
            .unwrap();

        assert!(
            worktree_dir.exists(),
            "outside-namespace worktree should not be removed"
        );
        let refusal_keys = daemon
            .scheduler
            .intervention_cooldowns
            .keys()
            .filter(|key| key.starts_with("removed-member-worktree-cleanup::outside-namespace::"))
            .count();
        assert_eq!(
            refusal_keys, 1,
            "repeated cleanup refusal should keep one cooldown entry"
        );
    }
}
