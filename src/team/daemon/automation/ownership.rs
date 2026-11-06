//! Resolution of conflicting engineer task ownership on the board.

use anyhow::Result;
use chrono::{DateTime, Utc};
use tracing::warn;

use super::super::*;
use super::*;

fn owned_task_status_rank(status: &str) -> u8 {
    match status {
        "in-progress" => 0,
        "todo" => 1,
        "backlog" => 2,
        _ => 3,
    }
}

fn owned_task_priority_rank(priority: &str) -> u32 {
    match priority {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 4,
    }
}

fn owned_task_collision_rank(task: &crate::task::Task) -> (u32, bool, Option<DateTime<Utc>>, u32) {
    (
        owned_task_priority_rank(&task.priority),
        task.claimed_at.is_none(),
        task.claimed_at.as_deref().and_then(parse_rfc3339_utc),
        task.id,
    )
}

pub(super) fn select_authoritative_multi_claim_task<'a>(
    claimed_tasks: &[&'a crate::task::Task],
) -> Option<&'a crate::task::Task> {
    claimed_tasks
        .iter()
        .copied()
        .min_by_key(|task| owned_task_collision_rank(task))
}

pub(super) fn select_authoritative_owned_task<'a>(
    current_task_id: Option<u32>,
    current_branch: Option<&str>,
    claimed_tasks: &[&'a crate::task::Task],
) -> Option<&'a crate::task::Task> {
    if let Some(task_id) = current_task_id
        && let Some(task) = claimed_tasks
            .iter()
            .copied()
            .find(|task| task.id == task_id)
    {
        return Some(task);
    }

    if let Some(branch) = current_branch {
        let mut matches = claimed_tasks
            .iter()
            .copied()
            .filter(|task| task.branch.as_deref() == Some(branch));
        if let Some(task) = matches.next()
            && matches.next().is_none()
        {
            return Some(task);
        }
    }

    claimed_tasks.iter().copied().min_by_key(|task| {
        (
            owned_task_status_rank(task.status.as_str()),
            owned_task_priority_rank(&task.priority),
            task.claimed_at.is_none(),
            task.claimed_at.as_deref().and_then(parse_rfc3339_utc),
            task.id,
        )
    })
}

impl TeamDaemon {
    pub(super) fn normalize_engineer_active_task_ownership(
        &mut self,
        board_tasks: &[crate::task::Task],
        board_dir: &std::path::Path,
        engineer: &str,
        audit_due: bool,
    ) -> Result<bool> {
        let claimed: Vec<&crate::task::Task> = board_tasks
            .iter()
            .filter(|task| {
                task.claimed_by.as_deref() == Some(engineer)
                    && super::super::interventions::task_needs_owned_intervention(
                        task.status.as_str(),
                    )
            })
            .collect();

        if claimed.is_empty() {
            return Ok(false);
        }

        let worktree_dir = self.worktree_dir(engineer);
        let current_branch =
            if self.states.get(engineer) == Some(&MemberState::Idle) && worktree_dir.exists() {
                current_worktree_branch(&worktree_dir).ok()
            } else {
                None
            };

        let branch_matches: Vec<u32> = current_branch
            .as_deref()
            .map(|branch| {
                claimed
                    .iter()
                    .copied()
                    .filter(|task| authoritative_task_branch(engineer, task) == branch)
                    .map(|task| task.id)
                    .collect()
            })
            .unwrap_or_default();
        let worktree_matches: Vec<u32> = claimed
            .iter()
            .copied()
            .filter(|task| {
                task.worktree_path.as_deref().is_some_and(|path| {
                    let candidate = std::path::PathBuf::from(path);
                    let resolved = if candidate.is_absolute() {
                        candidate
                    } else {
                        self.config.project_root.join(candidate)
                    };
                    resolved == worktree_dir
                })
            })
            .map(|task| task.id)
            .collect();

        let authoritative_id = if claimed.len() > 1 {
            self.active_task_id(engineer)
                .filter(|task_id| claimed.iter().any(|task| task.id == *task_id))
                .or_else(|| select_authoritative_multi_claim_task(&claimed).map(|task| task.id))
        } else if let [task_id] = branch_matches.as_slice() {
            Some(*task_id)
        } else if let [task_id] = worktree_matches.as_slice() {
            Some(*task_id)
        } else {
            select_authoritative_owned_task(
                self.active_task_id(engineer),
                current_branch.as_deref(),
                &claimed,
            )
            .map(|task| task.id)
        };
        let Some(authoritative_id) = authoritative_id else {
            return Ok(false);
        };

        if self.active_task_id(engineer) != Some(authoritative_id) {
            let reason = if claimed.len() == 1 {
                "adopt"
            } else {
                "repair"
            };
            self.active_tasks
                .insert(engineer.to_string(), authoritative_id);
            self.record_state_reconciliation(Some(engineer), Some(authoritative_id), reason);
            self.record_orchestrator_action(format!(
                "state reconciliation: set authoritative active task #{} for {}",
                authoritative_id, engineer
            ));
        }

        let mut released_excess_claims = false;
        if claimed.len() > 1 {
            for task in claimed
                .iter()
                .copied()
                .filter(|task| task.id != authoritative_id)
            {
                if verification_retry_required_task(task) {
                    continue;
                }
                warn!(
                    engineer,
                    task_id = task.id,
                    authoritative_task_id = authoritative_id,
                    "state reconciliation: releasing excess claimed task"
                );
                crate::team::task_cmd::reclaim_task_claim_with_attribution(
                    board_dir,
                    task.id,
                    &format!(
                        "Reclaimed during reconciliation while {engineer} retained authoritative task #{authoritative_id}."
                    ),
                    crate::team::task_cmd::StatusTransitionAttribution::daemon(
                        "daemon.automation.multi_claim_reconcile",
                    ),
                )?;
                released_excess_claims = true;
                self.record_state_reconciliation(Some(engineer), Some(task.id), "repair");
                self.record_orchestrator_action(format!(
                    "state reconciliation: released excess claimed task #{} from {} (keeping #{})",
                    task.id, engineer, authoritative_id
                ));
            }
        }

        if audit_due
            && let Some(task) = claimed
                .iter()
                .copied()
                .find(|task| task.id == authoritative_id)
        {
            self.maybe_align_engineer_worktree_with_task(engineer, task)?;
        }

        Ok(released_excess_claims)
    }
}
