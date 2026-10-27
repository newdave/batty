//! Board, task, review and dispatch commands.

use crate::{
    cli::{
        self, ActivityCommand, AutoMergeAction, BoardCommand, DepsFormatArg, ReviewDispositionArg,
        TaskCommand, TaskStateArg,
    },
    team,
};
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::path::Path;
fn task_state_arg_name(state: TaskStateArg) -> &'static str {
    match state {
        TaskStateArg::Backlog => "backlog",
        TaskStateArg::Todo => "todo",
        TaskStateArg::InProgress => "in-progress",
        TaskStateArg::Review => "review",
        TaskStateArg::Blocked => "blocked",
        TaskStateArg::Done => "done",
        TaskStateArg::Archived => "archived",
    }
}

fn review_disposition_arg_name(disposition: ReviewDispositionArg) -> &'static str {
    match disposition {
        ReviewDispositionArg::Approved => "approved",
        ReviewDispositionArg::ChangesRequested => "changes_requested",
        ReviewDispositionArg::Rejected => "rejected",
    }
}

fn board_summary_counts(board_dir: &std::path::Path) -> Result<Vec<(&'static str, usize)>> {
    const STATUSES: [&str; 7] = [
        "backlog",
        "todo",
        "in-progress",
        "review",
        "blocked",
        "done",
        "archived",
    ];

    STATUSES
        .into_iter()
        .map(|status| {
            let output = team::board_cmd::list_tasks(board_dir, Some(status))?;
            Ok((status, count_board_list_rows(&output)))
        })
        .collect()
}

fn count_board_list_rows(output: &str) -> usize {
    output
        .lines()
        .filter(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|token| token.chars().all(|ch| ch.is_ascii_digit()))
        })
        .count()
}

pub(super) fn run_board(root: &Path, command: Option<BoardCommand>) -> Result<()> {
    let board_dir = root.join(".batty").join("team_config").join("board");
    if !board_dir.is_dir() {
        bail!(
            "no board found at {}; run `batty init` first",
            board_dir.display()
        );
    }

    match command {
        Some(BoardCommand::List { status }) => {
            print!(
                "{}",
                team::board_cmd::list_tasks(&board_dir, status.as_deref())?
            );
        }
        Some(BoardCommand::Summary) => {
            for (status, count) in board_summary_counts(&board_dir)? {
                println!("{status:<11} {count}");
            }
        }
        Some(BoardCommand::Deps { format }) => {
            let fmt = match format {
                DepsFormatArg::Tree => team::deps::DepsFormat::Tree,
                DepsFormatArg::Flat => team::deps::DepsFormat::Flat,
                DepsFormatArg::Dot => team::deps::DepsFormat::Dot,
            };
            print!("{}", team::deps::render_deps(&board_dir, fmt)?);
        }
        Some(BoardCommand::Archive {
            older_than,
            dry_run,
        }) => {
            let max_age = team::board::parse_age_threshold(&older_than)?;
            let tasks = team::board::done_tasks_older_than(&board_dir, max_age)?;
            if dry_run {
                println!("[dry-run] Would archive {} task(s):", tasks.len());
            }
            let summary = team::board::archive_tasks(&board_dir, &tasks, dry_run)?;
            if !dry_run {
                println!(
                    "Archived {} task(s) to {}",
                    summary.archived_count,
                    summary.archive_dir.display()
                );
            }
        }
        Some(BoardCommand::Health) => {
            let events_path = root.join(".batty").join("team_config").join("events.jsonl");
            let health = team::board_health::compute_health(&board_dir, &events_path)?;
            print!("{}", team::board_health::format_health(&health));
        }
        None => {
            let status = std::process::Command::new("kanban-md")
                .args(["tui", "--dir", &board_dir.to_string_lossy()])
                .status()
                .context("failed to run kanban-md — is it installed?")?;
            if !status.success() {
                bail!("kanban-md tui failed");
            }
        }
    }
    Ok(())
}

pub(super) fn run_review(
    root: &Path,
    task_id: u32,
    disposition: cli::ReviewAction,
    feedback: Option<String>,
    reviewer: String,
) -> Result<()> {
    let board_dir = team::team_config_dir(root).join("board");
    let disposition_str = match disposition {
        cli::ReviewAction::Approve => "approve",
        cli::ReviewAction::RequestChanges => "request-changes",
        cli::ReviewAction::Reject => "reject",
    };
    team::task_cmd::cmd_review_structured(
        &board_dir,
        task_id,
        disposition_str,
        feedback.as_deref(),
        &reviewer,
    )?;
    Ok(())
}

pub(super) fn run_merge(root: &Path, engineer: String) -> Result<()> {
    team::merge_worktree(root, &engineer)?;
    Ok(())
}

pub(super) fn run_task(root: &Path, command: TaskCommand) -> Result<()> {
    let board_dir = team::team_config_dir(root).join("board");
    match command {
        TaskCommand::Transition {
            task_id,
            target_state,
        } => {
            team::task_cmd::cmd_transition(&board_dir, task_id, task_state_arg_name(target_state))?
        }
        TaskCommand::Assign {
            task_id,
            execution_owner,
            review_owner,
        } => team::task_cmd::cmd_assign(
            &board_dir,
            task_id,
            execution_owner.as_deref(),
            review_owner.as_deref(),
        )?,
        TaskCommand::Review {
            task_id,
            disposition,
            feedback,
        } => team::task_cmd::cmd_review(
            &board_dir,
            task_id,
            review_disposition_arg_name(disposition),
            feedback.as_deref(),
        )?,
        TaskCommand::Update {
            task_id,
            branch,
            commit,
            blocked_on,
            clear_blocked,
        } => {
            let mut fields = HashMap::new();
            if let Some(branch) = branch {
                fields.insert("branch".to_string(), branch);
            }
            if let Some(commit) = commit {
                fields.insert("commit".to_string(), commit);
            }
            if let Some(blocked_on) = blocked_on {
                fields.insert("blocked_on".to_string(), blocked_on);
            }
            if clear_blocked {
                fields.insert("clear_blocked".to_string(), "true".to_string());
            }
            team::task_cmd::cmd_update(&board_dir, task_id, fields)?;
        }
        TaskCommand::AutoMerge { task_id, action } => {
            let enabled = match action {
                AutoMergeAction::Enable => true,
                AutoMergeAction::Disable => false,
            };
            team::task_cmd::cmd_auto_merge(task_id, enabled, root)?;
        }
        TaskCommand::Schedule {
            task_id,
            at,
            cron,
            clear,
        } => team::task_cmd::cmd_schedule(
            &board_dir,
            task_id,
            at.as_deref(),
            cron.as_deref(),
            clear,
        )?,
    }
    Ok(())
}

pub(super) fn run_activity(root: &Path, command: ActivityCommand) -> Result<()> {
    let board_dir = team::team_config_dir(root).join("board");
    match command {
        ActivityCommand::AnnotateStatus { task_id, source } => {
            team::task_cmd::annotate_latest_status_activity_from_cli(&board_dir, task_id, &source)?;
        }
    }
    Ok(())
}

pub(super) fn run_queue(root: &Path) -> Result<()> {
    let entries = team::daemon::load_dispatch_queue_snapshot(root);
    if entries.is_empty() {
        println!("Dispatch queue is empty.");
    } else {
        println!(
            "{:<20} {:<8} {:<36} {:>8}  LAST FAILURE",
            "ENGINEER", "TASK", "TITLE", "FAILURES"
        );
        println!("{}", "-".repeat(100));
        for entry in entries {
            println!(
                "{:<20} {:<8} {:<36} {:>8}  {}",
                entry.engineer,
                entry.task_id,
                entry.task_title,
                entry.validation_failures,
                entry.last_failure.unwrap_or_else(|| "-".to_string())
            );
        }
    }
    Ok(())
}

pub(super) fn run_dispatch(root: &Path, explain: bool, task: Option<u32>) -> Result<()> {
    if !explain {
        println!("Use `batty dispatch --explain` to inspect the current routing decision.");
    } else {
        team::allocation::print_dispatch_explanation(root, task)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_list_counts_data_rows_only() {
        let output = "\
ID    STATUS        PRIORITY   TITLE    CLAIMED    TAGS    DUE
1     todo          high       One      --         --      --
12    review        medium     Two      @eng-1     dx      --
";

        assert_eq!(count_board_list_rows(output), 2);
    }

    #[test]
    fn board_list_ignores_headers_and_empty_output() {
        let output = "\
ID    STATUS        PRIORITY   TITLE    CLAIMED    TAGS    DUE
";

        assert_eq!(count_board_list_rows(output), 0);
        assert_eq!(count_board_list_rows(""), 0);
    }
}
