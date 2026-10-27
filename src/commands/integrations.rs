//! OpenClaw, Grafana, Discord and Telegram integrations.

use crate::{
    cli::{
        DiscordCommand, GrafanaCommand, OpenClawCommand, OpenClawEventTopicArg,
        OpenClawFollowUpCommand,
    },
    team,
};
use anyhow::Result;
use std::path::Path;

pub(super) fn run_open_claw(root: &Path, command: OpenClawCommand) -> Result<()> {
    match command {
        OpenClawCommand::Register { force } => {
            let path = team::openclaw::register_project(root, force)?;
            println!("OpenClaw project config written to {}", path.display());
        }
        OpenClawCommand::Status { json } => {
            team::openclaw::openclaw_status(root, json)?;
        }
        OpenClawCommand::Instruct { role, message } => {
            team::openclaw::send_openclaw_instruction(root, &role, &message)?;
            println!("OpenClaw instruction queued for {role}.");
        }
        OpenClawCommand::Events {
            project_id,
            all_projects,
            json,
            topics,
            roles,
            task_ids,
            event_types,
            session_names,
            since_ts,
            limit,
            include_archived,
        } => {
            let subscription = team::openclaw::OpenClawEventSubscription {
                topics: topics
                    .into_iter()
                    .map(|topic| match topic {
                        OpenClawEventTopicArg::Completion => {
                            team::openclaw::OpenClawEventTopic::Completion
                        }
                        OpenClawEventTopicArg::Review => team::openclaw::OpenClawEventTopic::Review,
                        OpenClawEventTopicArg::Stall => team::openclaw::OpenClawEventTopic::Stall,
                        OpenClawEventTopicArg::Merge => team::openclaw::OpenClawEventTopic::Merge,
                        OpenClawEventTopicArg::Escalation => {
                            team::openclaw::OpenClawEventTopic::Escalation
                        }
                        OpenClawEventTopicArg::DeliveryFailure => {
                            team::openclaw::OpenClawEventTopic::DeliveryFailure
                        }
                        OpenClawEventTopicArg::Lifecycle => {
                            team::openclaw::OpenClawEventTopic::Lifecycle
                        }
                    })
                    .collect(),
                project_ids: Vec::new(),
                session_names,
                roles,
                task_ids,
                event_types,
                since_ts,
                limit,
                include_archived,
            };
            team::openclaw::openclaw_events(
                root,
                &subscription,
                project_id.as_deref(),
                all_projects,
                json,
            )?;
        }
        OpenClawCommand::FollowUp { command } => match command {
            OpenClawFollowUpCommand::Run { json } => {
                team::openclaw::run_follow_ups(root, json)?;
            }
        },
    }
    Ok(())
}

pub(super) fn run_grafana(root: &Path, command: GrafanaCommand) -> Result<()> {
    let config_path = team::team_config_path(root);
    let port = if config_path.exists() {
        team::config::TeamConfig::load(&config_path)?.grafana.port
    } else {
        team::grafana::DEFAULT_PORT
    };
    match command {
        GrafanaCommand::Setup => team::grafana::setup(root, port)?,
        GrafanaCommand::Status => team::grafana::status(port)?,
        GrafanaCommand::Open => team::grafana::open(port)?,
    }
    Ok(())
}

pub(super) fn run_grafana_webhook(project_root: String, port: u16) -> Result<()> {
    team::grafana::run_alert_webhook(std::path::Path::new(&project_root), port)?;
    Ok(())
}

pub(super) fn run_discord(root: &Path, command: Option<DiscordCommand>) -> Result<()> {
    match command.unwrap_or(DiscordCommand::Setup) {
        DiscordCommand::Setup => team::setup_discord(root)?,
        DiscordCommand::Status => team::discord_status(root)?,
    }
    Ok(())
}

pub(super) fn run_telegram(root: &Path) -> Result<()> {
    team::setup_telegram(root)?;
    Ok(())
}
