//! Metrics, telemetry, retrospectives and exports.

use crate::{
    cli::{self},
    team,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
fn format_ts(unix_secs: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let dt = UNIX_EPOCH + Duration::from_secs(unix_secs as u64);
    let datetime: chrono::DateTime<chrono::Local> = dt.into();
    datetime.format("%Y-%m-%d %H:%M:%S").to_string()
}

pub(super) fn run_export_template(root: &Path, name: String) -> Result<()> {
    let count = team::export_template(root, &name)?;
    println!("Exported template '{name}' ({count} files)");
    Ok(())
}

pub(super) fn run_export_run(root: &Path) -> Result<()> {
    let path = team::export_run(root)?;
    println!("Run export written to {}", path.display());
    Ok(())
}

pub(super) fn run_retro(root: &Path, events: Option<PathBuf>) -> Result<()> {
    let stats = if let Some(events_path) = events {
        // Explicit path: use JSONL directly.
        team::retrospective::analyze_event_log(&events_path)?
    } else {
        // Default: prefer telemetry DB, fall back to JSONL.
        team::retrospective::analyze_project(root)?
    };
    match stats {
        Some(stats) => {
            let path = team::retrospective::generate_retrospective(root, &stats)?;
            println!("Retrospective written to {}", path.display());
        }
        None => println!("No run data found in event log."),
    }
    Ok(())
}

pub(super) fn run_metrics(root: &Path) -> Result<()> {
    team::metrics_cmd::run(root)?;
    Ok(())
}

pub(super) fn run_stress_test(
    root: &Path,
    compact: bool,
    duration_hours: u64,
    seed: u64,
    json_out: Option<PathBuf>,
    markdown_out: Option<PathBuf>,
) -> Result<()> {
    let report = team::stress::run(
        root,
        team::stress::StressTestOptions {
            compact,
            duration_hours,
            seed,
            json_out,
            markdown_out,
        },
    )?;
    println!(
        "Stress test complete: {} faults, {} failed SLA. JSON: {} Markdown: {}",
        report.summary.total_faults,
        report.summary.failed_faults,
        report.json_report_path.display(),
        report.markdown_report_path.display()
    );
    Ok(())
}

pub(super) fn run_telemetry(root: &Path, command: cli::TelemetryCommand) -> Result<()> {
    let conn = team::telemetry_db::open(root).context("failed to open telemetry database")?;
    match command {
        cli::TelemetryCommand::Summary => {
            let rows = team::telemetry_db::query_session_summaries(&conn)?;
            if rows.is_empty() {
                println!("No session summaries recorded yet.");
            } else {
                println!(
                    "{:<24} {:<20} {:<20} {:>10} {:>8} {:>8}",
                    "SESSION", "STARTED", "ENDED", "COMPLETED", "MERGES", "EVENTS"
                );
                for row in &rows {
                    let started = format_ts(row.started_at);
                    let ended = row
                        .ended_at
                        .map(format_ts)
                        .unwrap_or_else(|| "running".to_string());
                    println!(
                        "{:<24} {:<20} {:<20} {:>10} {:>8} {:>8}",
                        row.session_id,
                        started,
                        ended,
                        row.tasks_completed,
                        row.total_merges,
                        row.total_events
                    );
                }
            }
        }
        cli::TelemetryCommand::Agents => {
            let rows = team::telemetry_db::query_agent_metrics(&conn)?;
            if rows.is_empty() {
                println!("No agent metrics recorded yet.");
            } else {
                println!(
                    "{:<16} {:>11} {:>8} {:>8} {:>12} {:>8}",
                    "ROLE", "COMPLETIONS", "FAILURES", "RESTARTS", "CYCLE_SECS", "IDLE_PCT"
                );
                for row in &rows {
                    let total_polls = row.idle_polls + row.working_polls;
                    let idle_pct = if total_polls > 0 {
                        format!("{:.0}%", row.idle_polls as f64 / total_polls as f64 * 100.0)
                    } else {
                        "-".to_string()
                    };
                    println!(
                        "{:<16} {:>11} {:>8} {:>8} {:>12} {:>8}",
                        row.role,
                        row.completions,
                        row.failures,
                        row.restarts,
                        row.total_cycle_secs,
                        idle_pct
                    );
                }
            }
        }
        cli::TelemetryCommand::Tasks => {
            let rows = team::telemetry_db::query_task_metrics(&conn)?;
            if rows.is_empty() {
                println!("No task metrics recorded yet.");
            } else {
                println!(
                    "{:<8} {:<20} {:<20} {:>7} {:>11} {:>9} {:>10} {:>10}",
                    "TASK",
                    "STARTED",
                    "COMPLETED",
                    "RETRIES",
                    "ESCALATIONS",
                    "CTX_RST",
                    "MERGE_SECS",
                    "CONFIDENCE"
                );
                for row in &rows {
                    let started = row
                        .started_at
                        .map(format_ts)
                        .unwrap_or_else(|| "-".to_string());
                    let completed = row
                        .completed_at
                        .map(format_ts)
                        .unwrap_or_else(|| "-".to_string());
                    let merge = row
                        .merge_time_secs
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    let confidence = row
                        .confidence_score
                        .map(|c| format!("{:.2}", c))
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "{:<8} {:<20} {:<20} {:>7} {:>11} {:>9} {:>10} {:>10}",
                        row.task_id,
                        started,
                        completed,
                        row.retries,
                        row.escalations,
                        row.context_restart_count,
                        merge,
                        confidence
                    );
                }
            }
        }
        cli::TelemetryCommand::Reviews => {
            let row = team::telemetry_db::query_review_metrics(&conn)?;
            let total_merges = row.auto_merge_count + row.manual_merge_count;
            let auto_rate = if total_merges > 0 {
                format!(
                    "{:.0}%",
                    row.auto_merge_count as f64 / total_merges as f64 * 100.0
                )
            } else {
                "-".to_string()
            };
            let total_reviewed = total_merges + row.rework_count;
            let rework_rate = if total_reviewed > 0 {
                format!(
                    "{:.0}%",
                    row.rework_count as f64 / total_reviewed as f64 * 100.0
                )
            } else {
                "-".to_string()
            };
            let avg_latency = row
                .avg_review_latency_secs
                .map(|s| format!("{:.0}s", s))
                .unwrap_or_else(|| "-".to_string());
            println!("Review Pipeline (all sessions)");
            println!(
                "Auto-merge Rate: {} | Rework Rate: {}",
                auto_rate, rework_rate
            );
            println!(
                "Auto: {} | Manual: {} | Rework: {} | Nudges: {} | Escalations: {}",
                row.auto_merge_count,
                row.manual_merge_count,
                row.rework_count,
                row.review_nudge_count,
                row.review_escalation_count
            );
            println!("Avg Review Latency: {}", avg_latency);
        }
        cli::TelemetryCommand::Events { limit } => {
            let rows = team::telemetry_db::query_recent_events(&conn, limit)?;
            if rows.is_empty() {
                println!("No events recorded yet.");
            } else {
                println!(
                    "{:<20} {:<24} {:<16} {:<8}",
                    "TIMESTAMP", "EVENT", "ROLE", "TASK"
                );
                for row in &rows {
                    let ts = format_ts(row.timestamp);
                    let role = row.role.as_deref().unwrap_or("-");
                    let task = row.task_id.as_deref().unwrap_or("-");
                    println!("{:<20} {:<24} {:<16} {:<8}", ts, row.event_type, role, task);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn run_load(root: &Path) -> Result<()> {
    team::show_load(root)?;
    Ok(())
}

pub(super) fn run_parity(root: &Path, detail: bool, gaps: bool) -> Result<()> {
    team::parity::show_parity(root, detail, gaps)?;
    Ok(())
}

pub(super) fn run_cost(root: &Path, json: bool) -> Result<()> {
    if json {
        team::cost::show_cost_json(root)?;
    } else {
        team::cost::show_cost(root)?;
    }
    Ok(())
}

pub(super) fn run_outcome_record(
    root: &Path,
    task: u32,
    evidence: PathBuf,
    commit: Option<String>,
    session: Option<String>,
    defects: u32,
) -> Result<()> {
    team::outcomes::record(
        root,
        task,
        &evidence,
        commit.as_deref(),
        session.as_deref(),
        defects,
    )?;
    println!("Acceptance evidence recorded for task #{task}");
    Ok(())
}

pub(super) fn run_outcome_report(root: &Path, since: i64, until: Option<i64>) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&team::outcomes::report(
            root,
            since,
            until.unwrap_or_else(|| chrono::Utc::now().timestamp())
        )?)?
    );
    Ok(())
}
