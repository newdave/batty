//! Shared chat command dispatcher used by the Telegram and Discord bridges.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use tracing::info;

use super::super::*;

impl TeamDaemon {
    /// Execute a chat command. Commands that finish in the background (merge)
    /// send their final reply to `reply_to` when done.
    pub(in crate::team::daemon) fn execute_chat_command(
        &mut self,
        command: TelegramCommand,
        reply_to: background_tests::ChatReplyTarget,
    ) -> Result<String> {
        match command {
            TelegramCommand::Status => Ok(self.render_telegram_status_summary()),
            TelegramCommand::Board { filter } => {
                render_telegram_board_summary(&self.config.project_root, filter.as_deref())
            }
            TelegramCommand::Logs { member } => {
                render_telegram_logs(&self.config.project_root, &member)
            }
            TelegramCommand::Health => {
                render_telegram_health_summary(&self.config.project_root, &self.config.members)
            }
            TelegramCommand::Assign { engineer, task } => {
                self.execute_telegram_assign_command(&engineer, &task)
            }
            TelegramCommand::Merge { task_id } => {
                self.execute_telegram_merge_command(task_id, reply_to)
            }
            TelegramCommand::Kick { member } => self.execute_telegram_kick_command(&member),
            TelegramCommand::Pause => {
                crate::team::pause_team(&self.config.project_root)?;
                Ok("Automation paused.".to_string())
            }
            TelegramCommand::Resume => {
                crate::team::resume_team(&self.config.project_root)?;
                Ok("Automation resumed.".to_string())
            }
            TelegramCommand::Goal { text } => {
                write_telegram_goal(&self.config.project_root, &text)?;
                Ok(format!("Goal updated: {}", preview_text(&text, 180)))
            }
            TelegramCommand::Task { title } => {
                create_telegram_task(&self.config.project_root, &title)
            }
            TelegramCommand::Block { task_id, reason } => {
                block_telegram_task(&self.config.project_root, task_id, &reason)
            }
            TelegramCommand::Stop { confirm } => {
                if !confirm {
                    Ok("Reply with /stop confirm to stop the team.".to_string())
                } else {
                    let root = self.config.project_root.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                        let _ = crate::team::stop_team(&root);
                    });
                    Ok("Stopping team.".to_string())
                }
            }
            TelegramCommand::Start => {
                let session = format!("batty-{}", self.config.team_config.name);
                if crate::tmux::session_exists(&session) {
                    Ok(format!("Team already running: {session}"))
                } else {
                    let started = crate::team::start_team(&self.config.project_root, false)?;
                    Ok(format!("Started team: {started}"))
                }
            }
            TelegramCommand::Help => Ok(render_telegram_help()),
            TelegramCommand::Send { role, message } => {
                crate::team::messaging::send_message_as(
                    &self.config.project_root,
                    Some("human"),
                    &role,
                    &message,
                )?;
                Ok(format!("Sent to {role}: {}", preview_text(&message, 120)))
            }
        }
    }

    pub(in crate::team::daemon) fn render_telegram_status_summary(&self) -> String {
        let session = format!("batty-{}", self.config.team_config.name);
        let running = crate::tmux::session_exists(&session);
        let paused = crate::team::pause_marker_path(&self.config.project_root).exists();
        let triage_backlog_counts = crate::team::status::triage_backlog_counts(
            &self.config.project_root,
            &self.config.members,
        );
        let inbox_root = crate::team::inbox::inboxes_root(&self.config.project_root);
        let pending_inbox_total: usize = self
            .config
            .members
            .iter()
            .filter(|member| member.role_type != RoleType::User)
            .map(|member| {
                crate::team::inbox::pending_message_count(&inbox_root, &member.name).unwrap_or(0)
            })
            .sum();
        let triage_total: usize = triage_backlog_counts.values().sum();

        let mut state_counts = BTreeMap::new();
        for member in &self.config.members {
            if member.role_type == RoleType::User {
                continue;
            }
            let label = match self
                .states
                .get(&member.name)
                .copied()
                .unwrap_or(MemberState::Idle)
            {
                MemberState::Idle => "idle",
                MemberState::Working => "working",
            };
            *state_counts.entry(label).or_insert(0usize) += 1;
        }

        let states = if state_counts.is_empty() {
            "none".to_string()
        } else {
            state_counts
                .into_iter()
                .map(|(state, count)| format!("{state}={count}"))
                .collect::<Vec<_>>()
                .join(", ")
        };

        let (active_tasks, review_tasks) =
            crate::team::status::board_status_task_queues(&self.config.project_root)
                .unwrap_or_else(|_| (Vec::new(), Vec::new()));

        format!(
            "Team: {}\nSession: {}{}\nMembers: {}\nInbox: {}\nTriage: {}\nBoard: active={}, review={}",
            self.config.team_config.name,
            if running { "running" } else { "stopped" },
            if paused { " (paused)" } else { "" },
            states,
            pending_inbox_total,
            triage_total,
            active_tasks.len(),
            review_tasks.len(),
        )
    }

    pub(in crate::team::daemon) fn execute_telegram_assign_command(
        &mut self,
        engineer: &str,
        task: &str,
    ) -> Result<String> {
        let engineer_member = self
            .config
            .members
            .iter()
            .find(|member| member.name == engineer)
            .ok_or_else(|| anyhow!("Unknown engineer: {engineer}"))?;
        if engineer_member.role_type != RoleType::Engineer {
            bail!("{engineer} is not an engineer.");
        }
        if self.states.get(engineer) == Some(&MemberState::Working) {
            bail!("{engineer} is not idle.");
        }

        if let Some(task_id) = parse_task_id_arg(task) {
            let board_dir = self.board_dir();
            let task_path = crate::team::task_cmd::find_task_path(&board_dir, task_id)?;
            let task_record = crate::task::Task::from_file(&task_path)?;
            if !matches!(task_record.status.as_str(), "backlog" | "todo") {
                bail!(
                    "Task #{task_id} is not assignable from status '{}'.",
                    task_record.status
                );
            }
            if task_record.blocked.is_some() || task_record.blocked_on.is_some() {
                bail!("Task #{task_id} is blocked.");
            }

            crate::team::task_cmd::assign_task_owners(&board_dir, task_id, Some(engineer), None)?;
            if task_record.status == "backlog" {
                crate::team::task_cmd::transition_task_with_attribution(
                    &board_dir,
                    task_id,
                    "todo",
                    crate::team::task_cmd::StatusTransitionAttribution::bridge(
                        "bridge.telegram.assign",
                    ),
                )?;
            }
            crate::team::task_cmd::transition_task_with_attribution(
                &board_dir,
                task_id,
                "in-progress",
                crate::team::task_cmd::StatusTransitionAttribution::bridge(
                    "bridge.telegram.assign",
                ),
            )?;

            let inbox_id = crate::team::messaging::assign_task(
                &self.config.project_root,
                engineer,
                &format!("Task #{task_id}: {}", task_record.title),
            )?;
            Ok(format!(
                "Assigned {engineer} to #{task_id}: {}\nInbox id: {inbox_id}",
                preview_text(&task_record.title, 120)
            ))
        } else {
            let id =
                crate::team::messaging::assign_task(&self.config.project_root, engineer, task)?;
            Ok(format!("Assigned {engineer}: {task}\nInbox id: {id}"))
        }
    }

    /// Start a chat `merge`: validate now, run the verification command on a
    /// background thread, and merge + reply from the poll loop once it passes.
    pub(in crate::team::daemon) fn execute_telegram_merge_command(
        &mut self,
        task_id: u32,
        reply_to: background_tests::ChatReplyTarget,
    ) -> Result<String> {
        let board_dir = self.board_dir();
        let task_path = crate::team::task_cmd::find_task_path(&board_dir, task_id)?;
        let task = crate::task::Task::from_file(&task_path)?;
        if task.status != "review" {
            bail!("Task #{task_id} is not in review.");
        }
        if self.is_multi_repo {
            bail!("Merging from chat is not supported for multi-repo projects.");
        }
        if self.merge.queue.contains_task(task_id) {
            return Ok(format!("Task #{task_id} is already queued for merge."));
        }
        if self
            .verification
            .background_tests
            .chat_merges
            .iter()
            .any(|job| job.task_id == task_id)
        {
            return Ok(format!(
                "Task #{task_id} is already being verified for merge."
            ));
        }

        let engineer = engineer_for_merge_task(&self.config.project_root, task_id)?;
        let worktree_dir = self.worktree_dir(&engineer);
        let verification_policy = &self.config.team_config.workflow_policy.verification;
        let test_command = verification_policy.test_command.clone().or(self
            .config
            .team_config
            .workflow_policy
            .test_command
            .clone());
        let job = crate::team::test_job::TestJob::spawn_with_timeout(
            format!("chat-merge-{task_id}"),
            self.test_job_timeout(),
            move |child_pgid| {
                crate::team::task_loop::run_tests_in_worktree_tracked(
                    &worktree_dir,
                    test_command.as_deref(),
                    Some(child_pgid.as_ref()),
                )
            },
        );
        self.verification
            .background_tests
            .chat_merges
            .push(background_tests::ChatMergeJob {
                task_id,
                engineer: engineer.clone(),
                reply_to,
                job,
            });
        Ok(format!(
            "Running verification for Task #{task_id} from {engineer}; will queue the merge and reply when it passes."
        ))
    }

    /// Finish chat merges whose verification command has completed.
    pub(in crate::team::daemon) fn poll_chat_merge_jobs(&mut self) {
        let mut index = 0;
        while index < self.verification.background_tests.chat_merges.len() {
            let Some(result) = self.verification.background_tests.chat_merges[index]
                .job
                .try_take()
            else {
                index += 1;
                continue;
            };
            let job = self.verification.background_tests.chat_merges.remove(index);
            let duration_ms = job.job.elapsed().as_millis() as u64;
            info!(
                task_id = job.task_id,
                engineer = %job.engineer,
                duration_ms,
                "chat merge verification finished"
            );
            let reply =
                match self.finish_chat_merge(job.task_id, &job.engineer, result, duration_ms) {
                    Ok(reply) => reply,
                    Err(error) => format!("Command failed: {error}"),
                };
            self.integrations.send_chat_reply(&job.reply_to, &reply);
        }
    }

    /// Hand a chat merge whose verification passed to the merge queue, which
    /// takes the merge lock without blocking, serializes it with other
    /// merges, and runs post-merge verification.
    fn finish_chat_merge(
        &mut self,
        task_id: u32,
        engineer: &str,
        result: Result<crate::team::test_results::TestRunOutput>,
        test_duration_ms: u64,
    ) -> Result<String> {
        let test_run = result?;
        if !test_run.passed {
            bail!(
                "Task #{task_id} verification failed before merge.\n{}",
                preview_text(&test_run.results.failure_summary(), 240)
            );
        }

        // The board may have changed while the tests ran; re-check before
        // merging so a stale pass is never merged.
        let board_dir = self.board_dir();
        let task_path = crate::team::task_cmd::find_task_path(&board_dir, task_id)?;
        let task = crate::task::Task::from_file(&task_path)?;
        if task.status != "review"
            || engineer_for_merge_task(&self.config.project_root, task_id)? != engineer
        {
            info!(
                task_id,
                engineer,
                status = %task.status,
                "discarding stale chat merge verification: task changed while tests ran"
            );
            bail!("Task #{task_id} changed while its tests ran; merge skipped.");
        }

        let test_summary = test_run.results.summary.clone().unwrap_or_else(|| {
            format!(
                "{} passed, {} failed",
                test_run.results.passed, test_run.results.failed
            )
        });
        if self.merge.queue.contains_task(task_id) {
            return Ok(format!(
                "Task #{task_id} is already queued for merge. Tests: {test_summary}"
            ));
        }
        crate::team::merge::enqueue_reviewed_merge(
            self,
            task_id,
            engineer,
            test_run.results,
            test_duration_ms,
        )?;
        Ok(format!(
            "Queued Task #{task_id} from {engineer} for merge. Tests: {test_summary}"
        ))
    }

    pub(in crate::team::daemon) fn execute_telegram_kick_command(
        &mut self,
        member: &str,
    ) -> Result<String> {
        if self.active_task_id(member).is_some() {
            self.restart_member_with_task_context(member, "telegram kick")?;
            return Ok(format!("Restarted {member} with structured handoff."));
        }

        let mut saved = false;
        if self.member_uses_worktrees(member) {
            let worktree_dir = self.worktree_dir(member);
            if worktree_dir.exists() {
                saved = crate::worktree::ops::preserve_with_commit(
                    &worktree_dir,
                    "wip: auto-save before telegram kick [batty]",
                    std::time::Duration::from_secs(
                        self.config
                            .team_config
                            .workflow_policy
                            .graceful_shutdown_timeout_secs,
                    ),
                    "telegram kick",
                )?;
            }
        }

        let pane_id = self
            .config
            .pane_map
            .get(member)
            .ok_or_else(|| anyhow!("No pane registered for {member}."))?;
        crate::tmux::respawn_pane(pane_id, "bash")?;
        Ok(if saved {
            format!("Restarted {member}. Worktree auto-saved.")
        } else {
            format!("Restarted {member}.")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::team::daemon) enum TelegramCommand {
    Status,
    Board { filter: Option<String> },
    Logs { member: String },
    Health,
    Assign { engineer: String, task: String },
    Merge { task_id: u32 },
    Kick { member: String },
    Pause,
    Resume,
    Goal { text: String },
    Task { title: String },
    Block { task_id: u32, reason: String },
    Stop { confirm: bool },
    Start,
    Help,
    Send { role: String, message: String },
}

pub(super) fn render_telegram_board_summary(
    project_root: &std::path::Path,
    filter: Option<&str>,
) -> Result<String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        return Ok("Board: no tasks found.".to_string());
    }

    let tasks = crate::task::load_tasks_from_dir(&tasks_dir)?;
    if let Some(filter) = filter {
        let filtered: Vec<_> = tasks
            .iter()
            .filter(|task| task.status == filter)
            .take(8)
            .map(|task| match task.claimed_by.as_deref() {
                Some(owner) => format!("#{} {} ({owner})", task.id, preview_text(&task.title, 40)),
                None => format!("#{} {}", task.id, preview_text(&task.title, 40)),
            })
            .collect();
        let summary = if filtered.is_empty() {
            "none".to_string()
        } else {
            filtered.join("; ")
        };
        return Ok(format!("Board [{filter}]: {summary}"));
    }

    let mut counts = BTreeMap::<String, usize>::new();
    for task in &tasks {
        *counts.entry(task.status.clone()).or_insert(0) += 1;
    }

    let counts_summary = if counts.is_empty() {
        "empty".to_string()
    } else {
        counts
            .into_iter()
            .map(|(status, count)| format!("{status}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let (active_tasks, review_tasks) = crate::team::status::board_status_task_queues(project_root)?;
    let active_summary = summarize_status_entries(&active_tasks);
    let review_summary = summarize_status_entries(&review_tasks);

    Ok(format!(
        "Board: {counts_summary}\nActive: {active_summary}\nReview: {review_summary}"
    ))
}

fn render_telegram_logs(project_root: &std::path::Path, member: &str) -> Result<String> {
    let path = crate::team::shim_log_path(project_root, member);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("log not found for {member}: {}", path.display()))?;
    let lines: Vec<&str> = content.lines().rev().take(5).collect();
    if lines.is_empty() {
        return Ok(format!("{member}: no log output yet."));
    }
    let rendered = lines.into_iter().rev().collect::<Vec<_>>().join("\n");
    Ok(format!("{member} (last 5 lines)\n{rendered}"))
}

fn render_telegram_health_summary(
    project_root: &std::path::Path,
    members: &[crate::team::hierarchy::MemberInstance],
) -> Result<String> {
    let summary = crate::team::compute_session_summary(project_root);
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let metrics = crate::team::status::compute_metrics(&board_dir, members).unwrap_or_default();
    let lock_count = count_lock_files(project_root);
    Ok(format!(
        "Health\nRuntime: {}\nCompletions: {}\nMerges: {}\nReview queue: {}\nIn progress: {}\nRunnable: {}\nCargo locks: {}",
        summary
            .as_ref()
            .map(|s| s.runtime_secs.to_string())
            .unwrap_or_else(|| "0".to_string()),
        summary.as_ref().map(|s| s.tasks_completed).unwrap_or(0),
        summary.as_ref().map(|s| s.tasks_merged).unwrap_or(0),
        metrics.in_review_count,
        metrics.in_progress_count,
        metrics.runnable_count,
        lock_count,
    ))
}

fn count_lock_files(project_root: &std::path::Path) -> usize {
    let mut count = 0usize;
    let candidates = [
        project_root.join(".git").join("index.lock"),
        project_root
            .join(".batty")
            .join("shared-target")
            .join("Cargo.lock"),
    ];
    for path in candidates {
        if path.exists() {
            count += 1;
        }
    }
    count
}

fn write_telegram_goal(project_root: &std::path::Path, text: &str) -> Result<()> {
    let goal_path = project_root.join(".batty").join("goal.yaml");
    if let Some(parent) = goal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = format!("goal: {}\n", serde_yaml::to_string(text)?.trim());
    std::fs::write(goal_path, rendered)?;
    Ok(())
}

fn create_telegram_task(project_root: &std::path::Path, title: &str) -> Result<String> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let task_id = crate::team::board_cmd::create_task(
        &board_dir,
        title,
        "Created from Telegram.",
        Some("medium"),
        None,
        None,
    )
    .map_err(anyhow::Error::from)?;
    Ok(format!(
        "Created task #{task_id}: {}",
        preview_text(title, 120)
    ))
}

pub(super) fn block_telegram_task(
    project_root: &std::path::Path,
    task_id: u32,
    reason: &str,
) -> Result<String> {
    let board_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board");
    let mut fields = std::collections::HashMap::new();
    fields.insert("blocked_on".to_string(), reason.to_string());
    crate::team::task_cmd::cmd_update(&board_dir, task_id, fields)?;
    Ok(format!("Blocked #{task_id}: {}", preview_text(reason, 160)))
}

fn parse_task_id_arg(text: &str) -> Option<u32> {
    text.trim().trim_start_matches('#').parse::<u32>().ok()
}

fn render_telegram_help() -> String {
    "/status, /board [status], /logs <eng>, /health\n/assign <eng> <task|id>, /merge <task>, /kick <eng>, /pause, /resume\n/goal <text>, /task <title>, /block <task> <reason>, /stop [confirm], /start, /help, /send <role> <message>".to_string()
}

fn summarize_status_entries(entries: &[crate::team::status::StatusTaskEntry]) -> String {
    if entries.is_empty() {
        return "none".to_string();
    }

    entries
        .iter()
        .take(3)
        .map(|entry| {
            let base = match entry.claimed_by.as_deref() {
                Some(owner) => {
                    format!("#{} {} ({owner})", entry.id, preview_text(&entry.title, 32))
                }
                None => format!("#{} {}", entry.id, preview_text(&entry.title, 32)),
            };
            match entry.test_summary.as_deref() {
                Some(summary) => format!("{base} [{summary}]"),
                None => base,
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub(super) fn engineer_for_merge_task(
    project_root: &std::path::Path,
    task_id: u32,
) -> Result<String> {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    if !tasks_dir.is_dir() {
        bail!("Board not found.");
    }

    let task = crate::task::load_tasks_from_dir(&tasks_dir)?
        .into_iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| anyhow!("Task #{task_id} not found."))?;

    let engineer = task
        .claimed_by
        .ok_or_else(|| anyhow!("Task #{task_id} is not assigned."))?;
    if !engineer.starts_with("eng-") {
        bail!("Task #{task_id} is assigned to {engineer}, not an engineer.");
    }

    Ok(engineer)
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let preview: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{preview}...")
    } else {
        preview
    }
}
