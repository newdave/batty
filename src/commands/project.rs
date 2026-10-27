//! Global project registry commands.

use crate::{cli::ProjectCommand, project_registry};
use anyhow::{Result, bail};

pub(super) fn run_project(command: ProjectCommand) -> Result<()> {
    match command {
        ProjectCommand::Register {
            project_id,
            name,
            aliases,
            project_root,
            board_dir,
            team_name,
            session_name,
            owner,
            tags,
            channel_bindings,
            thread_bindings,
            allow_openclaw_supervision,
            allow_cross_project_routing,
            allow_shared_service_routing,
            archived,
            json,
        } => {
            let mut channel_bindings = channel_bindings
                .iter()
                .map(|binding| project_registry::parse_channel_binding(binding))
                .collect::<Result<Vec<_>>>()?;
            channel_bindings.extend(
                thread_bindings
                    .iter()
                    .map(|binding| project_registry::parse_thread_binding(binding))
                    .collect::<Result<Vec<_>>>()?,
            );
            let project =
                project_registry::register_project(project_registry::ProjectRegistration {
                    project_id,
                    name,
                    aliases,
                    project_root,
                    board_dir,
                    team_name,
                    session_name,
                    channel_bindings,
                    owner,
                    tags,
                    policy_flags: project_registry::ProjectPolicyFlags {
                        allow_openclaw_supervision,
                        allow_cross_project_routing,
                        allow_shared_service_routing,
                        archived,
                    },
                })?;
            if json {
                println!("{}", serde_json::to_string_pretty(&project)?);
            } else {
                println!(
                    "Registered project {} at {}",
                    project.project_id,
                    project.project_root.display()
                );
            }
        }
        ProjectCommand::Unregister { project_id, json } => {
            let Some(project) = project_registry::unregister_project(&project_id)? else {
                bail!("project '{}' is not registered", project_id);
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&project)?);
            } else {
                println!("Unregistered project {}", project.project_id);
            }
        }
        ProjectCommand::List { json } => {
            let projects = project_registry::list_projects()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&projects)?);
            } else if projects.is_empty() {
                println!("No projects registered.");
            } else {
                println!(
                    "{:<20} {:<24} {:<20} {:<18}",
                    "PROJECT_ID", "NAME", "TEAM", "SESSION"
                );
                for project in &projects {
                    println!(
                        "{:<20} {:<24} {:<20} {:<18}",
                        project.project_id, project.name, project.team_name, project.session_name
                    );
                }
            }
        }
        ProjectCommand::Get { project_id, json } => {
            let Some(project) = project_registry::get_project(&project_id)? else {
                bail!("project '{}' is not registered", project_id);
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&project)?);
            } else {
                println!("Project: {}", project.project_id);
                println!("Name: {}", project.name);
                println!("Project root: {}", project.project_root.display());
                println!("Board dir: {}", project.board_dir.display());
                println!("Team: {}", project.team_name);
                println!("Session: {}", project.session_name);
                println!("Owner: {}", project.owner.as_deref().unwrap_or("(unowned)"));
                println!(
                    "Tags: {}",
                    if project.tags.is_empty() {
                        "(none)".to_string()
                    } else {
                        project.tags.join(", ")
                    }
                );
                println!(
                    "Aliases: {}",
                    if project.aliases.is_empty() {
                        "(none)".to_string()
                    } else {
                        project.aliases.join(", ")
                    }
                );
            }
        }
        ProjectCommand::Start { project_id, json } => {
            let result = project_registry::start_project(&project_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.audit_message);
                println!(
                    "Lifecycle: {:?} | Running: {}",
                    result.lifecycle, result.running
                );
            }
        }
        ProjectCommand::Stop { project_id, json } => {
            let result = project_registry::stop_project(&project_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.audit_message);
                println!(
                    "Lifecycle: {:?} | Running: {}",
                    result.lifecycle, result.running
                );
            }
        }
        ProjectCommand::Restart { project_id, json } => {
            let result = project_registry::restart_project(&project_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}", result.audit_message);
                println!(
                    "Lifecycle: {:?} | Running: {}",
                    result.lifecycle, result.running
                );
            }
        }
        ProjectCommand::Status { project_id, json } => {
            let status = project_registry::get_project_status(&project_id)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Project: {}", status.project_id);
                println!("Name: {}", status.name);
                println!("Lifecycle: {:?}", status.lifecycle);
                println!("Running: {}", status.running);
                println!("Team: {}", status.team_name);
                println!("Session: {}", status.session_name);
                println!(
                    "Health: paused={} watchdog={} unhealthy={} triage_backlog={}",
                    status.health.paused,
                    status.health.watchdog_state,
                    status.health.unhealthy_members.len(),
                    status.health.triage_backlog_count
                );
                println!(
                    "Pipeline: active={} review={} runnable={} blocked={} stale_in_progress={} stale_review={}",
                    status.pipeline.active_task_count,
                    status.pipeline.review_queue_count,
                    status.pipeline.runnable_count,
                    status.pipeline.blocked_count,
                    status.pipeline.stale_in_progress_count,
                    status.pipeline.stale_review_count
                );
            }
        }
        ProjectCommand::SetActive {
            project_id,
            channel,
            binding,
            thread_binding,
            json,
        } => {
            let scope = match (channel, binding, thread_binding) {
                (None, None, None) => project_registry::ActiveProjectScope::Global,
                (Some(channel), Some(binding), None) => {
                    project_registry::ActiveProjectScope::Channel { channel, binding }
                }
                (Some(channel), Some(binding), Some(thread_binding)) => {
                    project_registry::ActiveProjectScope::Thread {
                        channel,
                        binding,
                        thread_binding,
                    }
                }
                _ => bail!(
                    "set-active requires either no scope, --channel with --binding, or --channel with --binding and --thread-binding"
                ),
            };
            let selection = project_registry::set_active_project(&project_id, scope)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&selection)?);
            } else {
                println!("Active project set to {}", selection.project_id);
            }
        }
        ProjectCommand::Resolve {
            message,
            channel,
            binding,
            thread_binding,
            json,
        } => {
            let decision = project_registry::resolve_project_for_message(
                &project_registry::ProjectRoutingRequest {
                    message,
                    channel,
                    binding,
                    thread_binding,
                },
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&decision)?);
            } else {
                println!(
                    "Selected: {}",
                    decision
                        .selected_project_id
                        .as_deref()
                        .unwrap_or("(clarification required)")
                );
                println!("Confidence: {:?}", decision.confidence);
                println!("Requires confirmation: {}", decision.requires_confirmation);
                println!("Reason: {}", decision.reason);
            }
        }
    }
    Ok(())
}
