//! Inbox and role-to-role messaging commands.

use crate::{cli::InboxCommand, team};
use anyhow::{Context, Result};
use std::path::Path;

pub(super) fn run_send(
    root: &Path,
    from: Option<String>,
    role: String,
    message: String,
) -> Result<()> {
    team::send_message_as(root, from.as_deref(), &role, &message)?;
    println!("Message queued for {role}.");
    Ok(())
}

pub(super) fn run_assign(root: &Path, engineer: String, task: String) -> Result<()> {
    let id = team::assign_task(root, &engineer, &task)?;
    println!(
        "Task queued for {engineer}. Inbox message id: {id}. Delivery result will be reported by Batty."
    );
    match team::wait_for_assignment_result(root, &id, std::time::Duration::from_secs(8))? {
        Some(result) => eprintln!("{}", team::format_assignment_result(&result)),
        None => eprintln!(
            "Assignment is still queued or pending delivery. No daemon result was available yet for {id}."
        ),
    }
    Ok(())
}

pub(super) fn run_inbox(
    root: &Path,
    command: Option<InboxCommand>,
    member: Option<String>,
    limit: usize,
    all: bool,
    raw: bool,
) -> Result<()> {
    match command {
        Some(InboxCommand::Purge {
            role,
            all_roles,
            before,
            older_than,
            all,
        }) => {
            let before = match (before, older_than) {
                (Some(ts), _) => Some(ts),
                (_, Some(dur)) => {
                    let age = team::board::parse_age_threshold(&dur)?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    Some(now.saturating_sub(age.as_secs()))
                }
                _ => None,
            };
            let summary = team::purge_inbox(root, role.as_deref(), all_roles, before, all)?;
            if all_roles {
                println!(
                    "Purged {} delivered message(s) across {} inbox(es).",
                    summary.messages, summary.roles
                );
            } else {
                let role = role.unwrap_or_default();
                println!(
                    "Purged {} delivered message(s) from {role}.",
                    summary.messages
                );
            }
        }
        None => {
            let member = member.context("member is required unless using `batty inbox purge`")?;
            let limit = if all { None } else { Some(limit) };
            team::list_inbox(root, &member, limit, raw)?;
        }
    }
    Ok(())
}

pub(super) fn run_read(root: &Path, member: String, id: String) -> Result<()> {
    team::read_message(root, &member, &id)?;
    Ok(())
}

pub(super) fn run_ack(root: &Path, member: String, id: String) -> Result<()> {
    team::ack_message(root, &member, &id)?;
    println!("Message {id} acknowledged for {member}.");
    Ok(())
}
