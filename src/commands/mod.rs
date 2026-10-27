//! Command handlers, one `run_<command>` function per CLI command, grouped by area.

mod board;
mod integrations;
mod internal;
mod lifecycle;
mod messaging;
mod metrics;
mod misc;
mod project;

use crate::cli::Command;
use crate::team;
use anyhow::Result;
use std::path::Path;

/// Run one parsed CLI command against the resolved project root.
pub fn dispatch(root: &Path, command: Command) -> Result<()> {
    match command {
        Command::Init {
            template,
            from,
            force,
            agent,
        } => lifecycle::run_init(root, template, from, force, agent)?,
        Command::ExportTemplate { name } => metrics::run_export_template(root, name)?,
        Command::ExportRun => metrics::run_export_run(root)?,
        Command::Retro { events } => metrics::run_retro(root, events)?,
        Command::Start { attach, quiet } => lifecycle::run_start(root, attach, quiet)?,
        Command::Stop => lifecycle::run_stop(root)?,
        Command::Attach { member } => lifecycle::run_attach(root, member.as_deref())?,
        Command::Status {
            json,
            detail,
            health,
            wide,
            no_color,
            watch,
        } => lifecycle::run_status(
            root,
            team::StatusOptions {
                json,
                detail,
                health,
                wide,
                no_color,
                watch,
            },
        )?,
        Command::Bench { engineer, reason } => lifecycle::run_bench(root, engineer, reason)?,
        Command::Unbench { engineer } => lifecycle::run_unbench(root, engineer)?,
        Command::OpenClaw { command } => integrations::run_open_claw(root, command)?,
        Command::Project { command } => project::run_project(command)?,
        Command::Watchdog {
            project_root,
            resume,
        } => internal::run_watchdog(project_root, resume)?,
        Command::Send {
            from,
            role,
            message,
        } => messaging::run_send(root, from, role, message)?,
        Command::Assign { engineer, task } => messaging::run_assign(root, engineer, task)?,
        Command::Validate { show_checks } => lifecycle::run_validate(root, show_checks)?,
        Command::Config { json } => lifecycle::run_config(root, json)?,
        Command::Board { command } => board::run_board(root, command)?,
        Command::Inbox {
            command,
            member,
            limit,
            all,
            raw,
        } => messaging::run_inbox(root, command, member, limit, all, raw)?,
        Command::Read { member, id } => messaging::run_read(root, member, id)?,
        Command::Ack { member, id } => messaging::run_ack(root, member, id)?,
        Command::Review {
            task_id,
            disposition,
            feedback,
            reviewer,
        } => board::run_review(root, task_id, disposition, feedback, reviewer)?,
        Command::Merge {
            engineer,
            force_clear_stale,
        } => board::run_merge(root, engineer, force_clear_stale)?,
        Command::Task { command } => board::run_task(root, command)?,
        Command::Activity { command } => board::run_activity(root, command)?,
        Command::Metrics => metrics::run_metrics(root)?,
        Command::StressTest {
            compact,
            duration_hours,
            seed,
            json_out,
            markdown_out,
        } => metrics::run_stress_test(root, compact, duration_hours, seed, json_out, markdown_out)?,
        Command::Telemetry { command } => metrics::run_telemetry(root, command)?,
        Command::Chat {
            agent_type,
            cmd,
            cwd,
            sdk_mode,
        } => internal::run_chat(agent_type, cmd, cwd, sdk_mode)?,
        Command::Shim {
            id,
            agent_type,
            cmd,
            cwd,
            rows,
            cols,
            pty_log_path,
            graceful_shutdown_timeout_secs,
            auto_commit_on_restart,
            sdk_mode,
        } => internal::run_shim(internal::ShimOptions {
            id,
            agent_type,
            cmd,
            cwd,
            rows,
            cols,
            pty_log_path,
            graceful_shutdown_timeout_secs,
            auto_commit_on_restart,
            sdk_mode,
        })?,
        Command::ConsolePane {
            project_root,
            member,
            events_log_path,
            pty_log_path,
        } => internal::run_console_pane(project_root, member, events_log_path, pty_log_path)?,
        Command::DaemonRestartIfStale { dry_run } => {
            lifecycle::run_daemon_restart_if_stale(root, dry_run)?
        }
        Command::Daemon {
            project_root,
            resume,
        } => internal::run_daemon(project_root, resume)?,
        Command::Completions { shell } => misc::run_completions(shell)?,
        Command::Nudge { command } => lifecycle::run_nudge(root, command)?,
        Command::Pause => lifecycle::run_pause(root)?,
        Command::Resume => lifecycle::run_resume(root)?,
        Command::Load => metrics::run_load(root)?,
        Command::Parity { detail, gaps } => metrics::run_parity(root, detail, gaps)?,
        Command::Verify => misc::run_verify(root)?,
        Command::Release { tag, readiness } => misc::run_release(root, tag, readiness)?,
        Command::Queue => board::run_queue(root)?,
        Command::Dispatch { explain, task } => board::run_dispatch(root, explain, task)?,
        Command::Cost { json } => metrics::run_cost(root, json)?,
        Command::Preflight { runtime } => lifecycle::run_preflight(root, runtime)?,
        Command::BackendReset { member } => lifecycle::run_backend_reset(root, member)?,
        Command::BackendProbe {
            member,
            timeout_secs,
        } => lifecycle::run_backend_probe(root, member, timeout_secs)?,
        Command::OutcomeRecord {
            task,
            evidence,
            commit,
            session,
            defects,
        } => metrics::run_outcome_record(root, task, evidence, commit, session, defects)?,
        Command::OutcomeReport { since, until } => metrics::run_outcome_report(root, since, until)?,
        Command::Scale { command } => lifecycle::run_scale(root, command)?,
        Command::Reload => lifecycle::run_reload(root)?,
        Command::Research { command } => misc::run_research(root, command)?,
        Command::Doctor { fix, yes } => lifecycle::run_doctor(root, fix, yes)?,
        Command::Worktree { health } => lifecycle::run_worktree(root, health)?,
        Command::Grafana { command } => integrations::run_grafana(root, command)?,
        Command::GrafanaWebhook { project_root, port } => {
            integrations::run_grafana_webhook(project_root, port)?
        }
        Command::Discord { command } => integrations::run_discord(root, command)?,
        Command::Telegram => integrations::run_telegram(root)?,
    }
    Ok(())
}
