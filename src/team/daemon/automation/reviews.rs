//! Stale review detection and escalation.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;

use super::super::*;
use super::*;

pub(super) fn current_review_task<'a>(
    tasks_by_id: &HashMap<u32, &'a crate::task::Task>,
    task_id: u32,
) -> Option<&'a crate::task::Task> {
    tasks_by_id
        .get(&task_id)
        .copied()
        .filter(|task| task.status == "review")
}

pub(super) fn active_stale_review_entries(
    report: &crate::team::board::TaskAgingReport,
    tasks_by_id: &HashMap<u32, &crate::task::Task>,
) -> (Vec<crate::team::board::AgedTask>, Vec<u32>) {
    let mut active = Vec::new();
    let mut suppressed = Vec::new();

    for task in &report.stale_review {
        if current_review_task(tasks_by_id, task.task_id).is_some() {
            active.push(task.clone());
        } else {
            suppressed.push(task.task_id);
        }
    }

    (active, suppressed)
}

fn sync_stale_review_next_action(
    task: &crate::task::Task,
    stale: &crate::team::review::StaleReviewState,
) -> Result<bool> {
    let next_action = stale.status_next_action();
    if task.next_action.as_deref() == Some(next_action.as_str()) {
        return Ok(false);
    }

    crate::team::task_cmd::update_task_frontmatter(&task.source_path, |mapping| {
        crate::team::task_cmd::set_optional_string(mapping, "next_action", Some(&next_action));
    })?;
    Ok(true)
}

impl TeamDaemon {
    pub(in crate::team::daemon) fn maybe_escalate_stale_reviews(&mut self) -> Result<()> {
        let board_dir = self.board_dir();
        let tasks_dir = board_dir.join("tasks");
        if !tasks_dir.exists() {
            return Ok(());
        }
        let tasks = self.board_tasks()?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Clone policy to avoid borrow conflict with &mut self methods below
        let policy = self.config.team_config.workflow_policy.clone();

        // Collect IDs of tasks currently in review
        let review_task_ids: HashSet<u32> = tasks
            .iter()
            .filter(|t| t.status == "review")
            .map(|t| t.id)
            .collect();

        // Prune tracking maps for tasks no longer in review and log the repair.
        let stale_review_first_seen: Vec<u32> = self
            .interventions
            .review_first_seen
            .keys()
            .copied()
            .filter(|id| !review_task_ids.contains(id))
            .collect();
        for task_id in stale_review_first_seen {
            self.interventions.review_first_seen.remove(&task_id);
            self.record_state_reconciliation(None, Some(task_id), "review_fix");
            self.record_orchestrator_action(format!(
                "state reconciliation: cleared stale review tracking for task #{}",
                task_id
            ));
        }
        let stale_review_nudges: Vec<u32> = self
            .interventions
            .review_nudge_sent
            .iter()
            .copied()
            .filter(|id| !review_task_ids.contains(id))
            .collect();
        for task_id in stale_review_nudges {
            self.interventions.review_nudge_sent.remove(&task_id);
            self.record_state_reconciliation(None, Some(task_id), "review_fix");
            self.record_orchestrator_action(format!(
                "state reconciliation: cleared stale review nudge state for task #{}",
                task_id
            ));
        }

        for task in tasks.iter() {
            if task.status != "review" {
                continue;
            }

            let review_state =
                crate::team::review::classify_review_task(&self.config.project_root, task, &tasks);
            if matches!(
                task.next_action.as_deref(),
                Some(next_action) if next_action.starts_with("stale review ->")
            ) && matches!(review_state, crate::team::review::ReviewQueueState::Current)
            {
                crate::team::task_cmd::update_task_frontmatter(&task.source_path, |mapping| {
                    crate::team::task_cmd::set_optional_string(mapping, "next_action", None);
                })?;
                self.record_state_reconciliation(None, Some(task.id), "review_fix");
                self.record_orchestrator_action(format!(
                    "state reconciliation: cleared stale review normalization hint for task #{}",
                    task.id
                ));
            }

            if !self.interventions.review_first_seen.contains_key(&task.id) {
                self.record_state_reconciliation(None, Some(task.id), "review_fix");
                self.record_orchestrator_action(format!(
                    "state reconciliation: seeded missing review tracking for task #{}",
                    task.id
                ));
                self.interventions.review_first_seen.insert(task.id, now);
            }
            let first_seen = *self
                .interventions
                .review_first_seen
                .get(&task.id)
                .unwrap_or(&now);
            let age = now.saturating_sub(first_seen);

            // Resolve per-priority thresholds (falls back to global defaults)
            let nudge_threshold =
                super::super::super::policy::effective_nudge_threshold(&policy, &task.priority);
            let timeout_threshold = super::super::super::policy::effective_escalation_threshold(
                &policy,
                &task.priority,
            );

            if let crate::team::review::ReviewQueueState::Stale(stale) = &review_state {
                if sync_stale_review_next_action(task, stale)? {
                    self.record_state_reconciliation(None, Some(task.id), "review_fix");
                    self.record_orchestrator_action(format!(
                        "state reconciliation: normalized stale review task #{} -> {} ({})",
                        task.id,
                        stale.next_step.as_str(),
                        stale.reason
                    ));
                }
                if !self.interventions.review_nudge_sent.contains(&task.id) {
                    let review_owner = task
                        .review_owner
                        .as_deref()
                        .map(str::to_string)
                        .or_else(|| {
                            task.claimed_by
                                .as_deref()
                                .and_then(|owner| self.manager_for_member_name(owner))
                                .map(str::to_string)
                        })
                        .or_else(|| first_manager_name(&self.config.members));
                    if let Some(review_owner) = review_owner.as_deref() {
                        let body = format!(
                            "Stale review normalization: task #{} no longer matches the live engineer lane.\nTask: {}\nReason: {}\nNext step: {}.",
                            task.id,
                            task.title,
                            stale.reason,
                            stale.status_next_action()
                        );
                        let _ = self.deliver_automation_nudge(review_owner, &body);
                    }
                    self.interventions.review_nudge_sent.insert(task.id);
                }
                continue;
            }

            // Check escalation first (higher threshold)
            if age >= timeout_threshold {
                // Escalate to architect
                let architect = self
                    .config
                    .members
                    .iter()
                    .find(|m| m.role_type == RoleType::Architect)
                    .map(|m| m.name.clone());

                if let Some(architect_name) = architect {
                    let msg = format!(
                        "Review timeout: task #{} has been in review for {}s (threshold: {}s). \
                         Escalating for resolution.",
                        task.id, age, timeout_threshold,
                    );
                    let _ = self.queue_daemon_message(&architect_name, &msg);
                    self.record_orchestrator_action(format!(
                        "review_escalated: task #{} -> {architect_name}",
                        task.id,
                    ));
                }

                self.emit_event(TeamEvent::review_escalated(
                    &task.id.to_string(),
                    &format!("review timeout after {age}s"),
                ));

                // Transition to blocked
                let _ = super::super::super::task_cmd::transition_task_with_attribution(
                    &board_dir,
                    task.id,
                    "blocked",
                    super::super::super::task_cmd::StatusTransitionAttribution::daemon(
                        "daemon.automation.stale_review",
                    ),
                );
                let _ = super::super::super::task_cmd::cmd_update(
                    &board_dir,
                    task.id,
                    std::collections::HashMap::from([(
                        "blocked_on".to_string(),
                        "review timeout escalated to architect".to_string(),
                    )]),
                );

                // Remove from tracking since it's no longer in review
                self.interventions.review_first_seen.remove(&task.id);
                self.interventions.review_nudge_sent.remove(&task.id);
                continue;
            }

            // Check nudge threshold
            if age >= nudge_threshold && !self.interventions.review_nudge_sent.contains(&task.id) {
                let reviewer = task.review_owner.as_deref().unwrap_or("manager");
                let msg = format!(
                    "Review nudge: task #{} has been in review for {}s (nudge threshold: {}s). \
                     Please review or escalate.",
                    task.id, age, nudge_threshold,
                );
                let _ = self.deliver_automation_nudge(reviewer, &msg);
                self.record_orchestrator_action(format!(
                    "review_nudge_sent: task #{} -> {reviewer}",
                    task.id,
                ));

                self.emit_event(TeamEvent::review_nudge_sent(reviewer, &task.id.to_string()));

                self.interventions.review_nudge_sent.insert(task.id);
            }
        }

        Ok(())
    }
}
