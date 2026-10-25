//! Codex SDK-mode shim runtime: communicates with Codex via JSONL events
//! from `codex exec --json` instead of screen-scraping a PTY.
//!
//! Unlike the Claude SDK runtime (persistent subprocess with stdin NDJSON),
//! Codex uses a **spawn-per-message** model: each `SendMessage` launches a
//! new `codex exec --json` subprocess. Multi-turn context is preserved via
//! `codex exec resume <thread_id>`.
//!
//! Emits the same `Command`/`Event` protocol to the orchestrator as the
//! PTY runtime, making it transparent to all upstream consumers.

use std::collections::VecDeque;
use std::io::Write;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::TimeZone;
use regex::Regex;

use super::codex_types::{self, CodexEvent};
use super::common::{
    self, MAX_QUEUE_DEPTH, QueuedMessage, SESSION_STATS_INTERVAL_SECS, drain_queue_errors,
    format_injected_message,
};
use super::protocol::{Channel, Command as ShimCommand, Event, ShimState};
use super::pty_log::PtyLogWriter;
use super::runtime::ShimArgs;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

const PROCESS_EXIT_POLL_MS: u64 = 100;
const GROUP_TERM_GRACE_SECS: u64 = 2;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct CodexState {
    state: ShimState,
    state_changed_at: Instant,
    started_at: Instant,
    /// Thread ID from the first `thread.started` event, used for resume.
    thread_id: Option<String>,
    launch_context: super::codex_session::LaunchContext,
    /// Accumulated agent response text for the current turn.
    accumulated_response: String,
    /// Message ID of the currently pending (in-flight) message.
    pending_message_id: Option<String>,
    /// Messages queued while the agent is in Working state.
    message_queue: VecDeque<QueuedMessage>,
    /// Total bytes of response text received.
    cumulative_output_bytes: u64,
    input_tokens: u64,
    output_tokens: u64,
    active_child: Option<Arc<Mutex<Child>>>,
    /// The codex binary name/path.
    program: String,
    /// Working directory for spawning subprocesses.
    cwd: std::path::PathBuf,
    /// Whether a ContextApproaching event has already been emitted this session.
    context_approaching_emitted: bool,
    /// When a backend quota block expires, if known.
    quota_blocked_until: Option<u64>,
    configuration_error: Option<(String, Option<String>)>,
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

/// Run the Codex SDK-mode shim. This function does not return until the shim exits.
///
/// Unlike the Claude SDK runtime, each message spawns a new `codex exec --json`
/// subprocess. The shim manages the lifecycle per-message and emits the same
/// `Command`/`Event` protocol to the orchestrator.
pub fn run_codex_sdk(args: ShimArgs, channel: Channel) -> Result<()> {
    eprintln!("[shim-codex {}] started (spawn-per-message mode)", args.id);

    let launch_context = super::codex_session::LaunchContext::load(&args.cwd, &args.id)?;

    // Shared state
    let state = Arc::new(Mutex::new(CodexState {
        state: ShimState::Idle,
        state_changed_at: Instant::now(),
        started_at: Instant::now(),
        thread_id: launch_context.resume_session_id.clone(),
        launch_context,
        accumulated_response: String::new(),
        pending_message_id: None,
        message_queue: VecDeque::new(),
        cumulative_output_bytes: 0,
        input_tokens: 0,
        output_tokens: 0,
        active_child: None,
        program: "codex".to_string(),
        cwd: args.cwd.clone(),
        context_approaching_emitted: false,
        quota_blocked_until: None,
        configuration_error: None,
    }));

    // PTY log writer (optional — writes readable text for tmux display)
    let pty_log: Option<Arc<Mutex<PtyLogWriter>>> = args
        .pty_log_path
        .as_deref()
        .map(|p| PtyLogWriter::new(p).context("failed to create PTY log"))
        .transpose()?
        .map(|w| Arc::new(Mutex::new(w)));

    // Channel clones
    let mut cmd_channel = channel;

    // Emit Ready immediately — no persistent subprocess to wait for.
    cmd_channel.send(&Event::Ready)?;

    // Session stats thread
    let state_stats = Arc::clone(&state);
    let mut stats_channel = cmd_channel
        .try_clone()
        .context("failed to clone channel for stats")?;
    thread::spawn(move || {
        loop {
            thread::sleep(Duration::from_secs(SESSION_STATS_INTERVAL_SECS));
            let st = state_stats.lock().unwrap();
            if st.state == ShimState::Dead {
                return;
            }
            let output_bytes = st.cumulative_output_bytes;
            let input_tokens = st.input_tokens;
            let output_tokens = st.output_tokens;
            let uptime_secs = st.started_at.elapsed().as_secs();
            drop(st);

            if stats_channel
                .send(&Event::SessionStats {
                    output_bytes,
                    uptime_secs,
                    input_tokens,
                    output_tokens,
                    context_usage_pct: None,
                })
                .is_err()
            {
                return;
            }
        }
    });

    // Command loop (main thread)
    let state_cmd = Arc::clone(&state);
    let shim_id = args.id.clone();
    loop {
        let cmd = match cmd_channel.recv::<ShimCommand>() {
            Ok(Some(c)) => c,
            Ok(None) => {
                eprintln!("[shim-codex {shim_id}] orchestrator disconnected");
                break;
            }
            Err(e) => {
                eprintln!("[shim-codex {shim_id}] channel error: {e}");
                break;
            }
        };

        match cmd {
            ShimCommand::SendMessage {
                from,
                body,
                message_id,
            } => {
                let delivery_id = message_id.clone();
                let mut st = state_cmd.lock().unwrap();
                if let Some((message, code)) = st.configuration_error.clone() {
                    drop(st);
                    cmd_channel.send(&Event::ConfigurationRequired { message, code })?;
                    continue;
                }
                if let Some(until) = st.quota_blocked_until {
                    let now = current_unix_secs();
                    if until > now {
                        let retry_at_label = format_quota_retry_label(until);
                        let reason = format!(
                            "backend quota is blocked until {retry_at_label}; message not sent"
                        );
                        drop(st);
                        cmd_channel.send(&Event::QuotaBlocked {
                            message: reason.clone(),
                            retry_at_epoch_secs: Some(until),
                            retry_at_label: Some(retry_at_label),
                        })?;
                        continue;
                    }
                    st.quota_blocked_until = None;
                }
                match st.state {
                    ShimState::Idle => {
                        st.pending_message_id = message_id;
                        st.accumulated_response.clear();
                        st.state = ShimState::Working;
                        st.state_changed_at = Instant::now();
                        let thread_id = st.thread_id.clone();
                        let program = st.program.clone();
                        let cwd = st.cwd.clone();
                        let text = st.launch_context.prompt(
                            st.thread_id.is_none(),
                            &format_injected_message(&from, &body),
                        );
                        st.context_approaching_emitted = false;
                        drop(st);

                        cmd_channel.send(&Event::StateChanged {
                            from: ShimState::Idle,
                            to: ShimState::Working,
                            summary: String::new(),
                        })?;
                        if let Some(id) = delivery_id {
                            cmd_channel.send(&Event::MessageDelivered { id })?;
                        }

                        // Spawn codex exec subprocess for this message
                        let (exec_program, exec_args) =
                            codex_types::codex_sdk_args(&program, thread_id.as_deref());

                        let mut evt_channel = cmd_channel
                            .try_clone()
                            .context("failed to clone channel for codex exec")?;
                        let state_exec = Arc::clone(&state_cmd);
                        let pty_log_exec = pty_log.clone();
                        let shim_id_exec = shim_id.clone();

                        // Run the codex exec subprocess in a background thread
                        thread::spawn(move || {
                            run_codex_exec(
                                &shim_id_exec,
                                &exec_program,
                                &exec_args,
                                &text,
                                &cwd,
                                &state_exec,
                                &mut evt_channel,
                                pty_log_exec.as_ref(),
                            );
                        });
                    }
                    ShimState::Working => {
                        // Queue the message
                        if st.message_queue.len() >= MAX_QUEUE_DEPTH {
                            let dropped = st.message_queue.pop_front();
                            let dropped_id = dropped.as_ref().and_then(|m| m.message_id.clone());
                            st.message_queue.push_back(QueuedMessage {
                                from,
                                body,
                                message_id,
                            });
                            let depth = st.message_queue.len();
                            drop(st);

                            cmd_channel.send(&Event::Error {
                                command: "SendMessage".into(),
                                reason: format!(
                                    "message queue full ({MAX_QUEUE_DEPTH}), dropped oldest message{}",
                                    dropped_id
                                        .map(|id| format!(" (id: {id})"))
                                        .unwrap_or_default(),
                                ),
                            })?;
                            cmd_channel.send(&Event::Warning {
                                message: format!(
                                    "message queued while agent working (depth: {depth})"
                                ),
                                idle_secs: None,
                            })?;
                        } else {
                            st.message_queue.push_back(QueuedMessage {
                                from,
                                body,
                                message_id,
                            });
                            let depth = st.message_queue.len();
                            drop(st);

                            cmd_channel.send(&Event::Warning {
                                message: format!(
                                    "message queued while agent working (depth: {depth})"
                                ),
                                idle_secs: None,
                            })?;
                        }
                    }
                    other => {
                        drop(st);
                        cmd_channel.send(&Event::Error {
                            command: "SendMessage".into(),
                            reason: format!("agent in {other} state, cannot accept message"),
                        })?;
                    }
                }
            }

            ShimCommand::CaptureScreen { last_n_lines } => {
                let st = state_cmd.lock().unwrap();
                let content = match last_n_lines {
                    Some(n) => last_n_lines_of(&st.accumulated_response, n),
                    None => st.accumulated_response.clone(),
                };
                drop(st);
                cmd_channel.send(&Event::ScreenCapture {
                    content,
                    cursor_row: 0,
                    cursor_col: 0,
                })?;
            }

            ShimCommand::GetState => {
                let st = state_cmd.lock().unwrap();
                let since = st.state_changed_at.elapsed().as_secs();
                let state = st.state;
                drop(st);
                cmd_channel.send(&Event::State {
                    state,
                    since_secs: since,
                })?;
            }

            ShimCommand::Resize { .. } => {
                // No-op — no PTY.
            }

            ShimCommand::Ping => {
                cmd_channel.send(&Event::Pong)?;
            }

            ShimCommand::Shutdown { reason, .. } => {
                eprintln!(
                    "[shim-codex {shim_id}] shutdown requested ({})",
                    reason.label()
                );
                stop_codex_exec(&state_cmd);
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!(
                        "[shim-codex {shim_id}] failed to preserve work before shutdown: {error:#}"
                    );
                }
                let mut st = state_cmd.lock().unwrap();
                st.state = ShimState::Dead;
                st.state_changed_at = Instant::now();
                drop(st);
                break;
            }

            ShimCommand::Kill => {
                stop_codex_exec(&state_cmd);
                if let Err(error) = super::runtime::preserve_work_before_kill(&args.cwd) {
                    eprintln!(
                        "[shim-codex {shim_id}] failed to preserve work before kill: {error:#}"
                    );
                }
                let mut st = state_cmd.lock().unwrap();
                st.state = ShimState::Dead;
                st.state_changed_at = Instant::now();
                drop(st);
                break;
            }
        }
    }

    stop_codex_exec(&state_cmd);
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-message subprocess execution
// ---------------------------------------------------------------------------

/// Spawn `codex exec --json ...` and process its JSONL output.
/// When the subprocess exits, transition back to Idle and drain the queue.
#[allow(clippy::too_many_arguments)]
fn run_codex_exec(
    shim_id: &str,
    program: &str,
    args: &[String],
    prompt: &str,
    cwd: &std::path::Path,
    state: &Arc<Mutex<CodexState>>,
    evt_channel: &mut Channel,
    pty_log: Option<&Arc<Mutex<PtyLogWriter>>>,
) {
    // Spawn the subprocess
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CLAUDECODE");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut start_guard = state.lock().unwrap();
    if start_guard.state == ShimState::Dead {
        return;
    }
    let child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            drop(start_guard);
            eprintln!("[shim-codex {shim_id}] failed to spawn codex exec: {e}");
            let mut st = state.lock().unwrap();
            let msg_id = st.pending_message_id.take();
            let drained = fail_queued_messages(&mut st.message_queue);
            st.state = ShimState::Idle;
            st.state_changed_at = Instant::now();
            drop(st);
            let _ = evt_channel.send(&Event::Error {
                command: "SendMessage".into(),
                reason: format!("codex exec spawn failed: {e}"),
            });
            let _ = evt_channel.send(&Event::StateChanged {
                from: ShimState::Working,
                to: ShimState::Idle,
                summary: format!("spawn failed: {e}"),
            });
            if let Some(id) = msg_id {
                let _ = evt_channel.send(&Event::DeliveryFailed {
                    id,
                    reason: "Codex process could not start".into(),
                });
            }
            for event in drained {
                let _ = evt_channel.send(&event);
            }
            return;
        }
    };

    let child = Arc::new(Mutex::new(child));
    start_guard.active_child = Some(Arc::clone(&child));
    drop(start_guard);
    let _execution = CodexExecGuard {
        child: Arc::clone(&child),
        state: Arc::clone(state),
    };
    let child_pid = child.lock().unwrap().id();
    eprintln!("[shim-codex {shim_id}] codex exec spawned (pid {child_pid})");

    let stdin = child.lock().unwrap().stdin.take();
    if let Some(mut stdin) = stdin {
        if let Err(e) = stdin.write_all(prompt.as_bytes()) {
            eprintln!("[shim-codex {shim_id}] failed to write prompt to stdin: {e}");
        }
    }

    let stdout = child.lock().unwrap().stdout.take().unwrap();
    let stderr = child.lock().unwrap().stderr.take().unwrap();

    // stderr reader (log only). Non-recoverable auth failures are detected
    // in the structured `turn.failed` / `error` events below — the codex
    // CLI surfaces the same message there, so we don't need to race the
    // stderr thread against the stdout JSONL path.
    let shim_id_err = shim_id.to_string();
    let pty_log_err = pty_log.map(Arc::clone);
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line_result in reader.lines() {
            match line_result {
                Ok(line) => {
                    eprintln!("[shim-codex {shim_id_err}] stderr: {line}");
                    if let Some(ref log) = pty_log_err {
                        let _ = log
                            .lock()
                            .unwrap()
                            .write(format!("[stderr] {line}\n").as_bytes());
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut turn_usage = super::codex_types::CodexUsage::default();
    let mut turn_completed = false;

    // stdout JSONL reader
    let reader = BufReader::new(stdout);
    for line_result in reader.lines() {
        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[shim-codex {shim_id}] stdout read error: {e}");
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        // Keep raw structured error codes: the compatibility event type only
        // models the message and otherwise ignores future provider fields.
        let raw: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        if matches!(
            raw.get("type").and_then(|v| v.as_str()),
            Some("turn.failed" | "error")
        ) {
            let message = raw
                .pointer("/error/message")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("message").and_then(|v| v.as_str()))
                .unwrap_or("unknown error");
            if let Some(code) = model_configuration_error(&raw, message) {
                let mut st = state.lock().unwrap();
                st.pending_message_id = None;
                st.state = ShimState::Idle;
                st.state_changed_at = Instant::now();
                st.configuration_error = Some((message.to_string(), code.clone()));
                let drain = drain_queue_errors(&mut st.message_queue, ShimState::Idle);
                drop(st);
                let _ = evt_channel.send(&Event::ConfigurationRequired {
                    message: message.into(),
                    code,
                });
                for event in drain {
                    let _ = evt_channel.send(&event);
                }
                // The reader must not wait indefinitely for a terminally
                // misconfigured provider process that leaves stdout open.
                return;
            }
        }

        let evt: CodexEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[shim-codex {shim_id}] ignoring unparseable JSONL: {e}");
                continue;
            }
        };

        match evt.event_type.as_str() {
            "thread.started" => {
                if let Some(tid) = evt.thread_id {
                    let mut st = state.lock().unwrap();
                    let invalid = uuid::Uuid::parse_str(&tid).is_err()
                        || st
                            .thread_id
                            .as_ref()
                            .is_some_and(|existing| existing != &tid);
                    if invalid {
                        st.configuration_error = Some((
                            "Codex thread identity changed unexpectedly".into(),
                            Some("thread_identity_mismatch".into()),
                        ));
                        drop(st);
                        let _ = evt_channel.send(&Event::ConfigurationRequired {
                            message: "Codex thread identity changed unexpectedly".into(),
                            code: Some("thread_identity_mismatch".into()),
                        });
                        return;
                    }
                    st.thread_id = Some(tid.clone());
                    st.launch_context.resume_session_id = Some(tid.clone());
                    let context = st.launch_context.clone();
                    drop(st);
                    if let Err(error) = context.save(cwd, shim_id) {
                        let _ = evt_channel.send(&Event::Error {
                            command: "PersistCodexThread".into(),
                            reason: error.to_string(),
                        });
                    }
                    let _ = evt_channel.send(&Event::SessionStarted { session_id: tid });
                }
            }

            "item.completed" | "item.updated" => {
                if let Some(ref item) = evt.item {
                    if let Some(text) = item.agent_text() {
                        if !text.is_empty() {
                            let mut st = state.lock().unwrap();
                            // Replace accumulated response on each complete agent_message
                            // (Codex sends the full text each time, not deltas)
                            if evt.event_type == "item.completed" {
                                st.accumulated_response = text.to_string();
                            }
                            st.cumulative_output_bytes += text.len() as u64;

                            // Check for context approaching limit (proactive).
                            if !st.context_approaching_emitted
                                && common::detect_context_approaching_limit(text)
                            {
                                st.context_approaching_emitted = true;
                                drop(st);
                                let _ = evt_channel.send(&Event::ContextApproaching {
                                    message: "Agent output contains context-pressure signals"
                                        .into(),
                                    input_tokens: 0,
                                    output_tokens: 0,
                                });
                            } else {
                                drop(st);
                            }

                            if let Some(log) = pty_log {
                                let _ = log.lock().unwrap().write(text.as_bytes());
                                let _ = log.lock().unwrap().write(b"\n");
                            }
                        }
                    }
                }
            }

            "turn.failed" => {
                let error_msg = evt.error_message().unwrap_or("unknown error").to_string();
                eprintln!("[shim-codex {shim_id}] turn failed: {error_msg}");

                // Non-recoverable auth failure: the codex refresh token has
                // been consumed. No amount of respawning fixes this — a
                // human must re-login. Park the shim Idle and surface an
                // AuthRequired event so the daemon stops dispatching.
                if detect_auth_required(&error_msg) || structured_auth_required(&raw) {
                    let mut st = state.lock().unwrap();
                    st.pending_message_id = None;
                    st.state = ShimState::Idle;
                    st.state_changed_at = Instant::now();
                    let drain = drain_queue_errors(&mut st.message_queue, ShimState::Idle);
                    drop(st);

                    let _ = evt_channel.send(&Event::AuthRequired { message: error_msg });
                    for event in drain {
                        let _ = evt_channel.send(&event);
                    }
                    return;
                }

                // Check for context exhaustion
                if common::detect_context_exhausted(&error_msg) {
                    let mut st = state.lock().unwrap();
                    let last_lines = last_n_lines_of(&st.accumulated_response, 5);
                    st.state = ShimState::ContextExhausted;
                    st.state_changed_at = Instant::now();
                    let drain =
                        drain_queue_errors(&mut st.message_queue, ShimState::ContextExhausted);
                    drop(st);

                    let _ = evt_channel.send(&Event::StateChanged {
                        from: ShimState::Working,
                        to: ShimState::ContextExhausted,
                        summary: last_lines.clone(),
                    });
                    let _ = evt_channel.send(&Event::ContextExhausted {
                        message: error_msg,
                        last_lines,
                    });
                    for event in drain {
                        let _ = evt_channel.send(&event);
                    }
                    return;
                }

                if let Some(quota_block) = detect_quota_block(&error_msg) {
                    let mut st = state.lock().unwrap();
                    st.pending_message_id = None;
                    st.state = ShimState::Idle;
                    st.state_changed_at = Instant::now();
                    st.quota_blocked_until = quota_block.retry_at_epoch_secs;
                    let drain = drain_queue_errors(&mut st.message_queue, ShimState::Idle);
                    drop(st);

                    let _ = evt_channel.send(&Event::QuotaBlocked {
                        message: error_msg.clone(),
                        retry_at_epoch_secs: quota_block.retry_at_epoch_secs,
                        retry_at_label: quota_block.retry_at_label.clone(),
                    });
                    for event in drain {
                        let _ = evt_channel.send(&event);
                    }
                    return;
                }
            }

            "error" => {
                let error_msg = evt.error_message().unwrap_or("stream error").to_string();
                eprintln!("[shim-codex {shim_id}] error event: {error_msg}");

                // Non-recoverable auth failure short-circuits before quota
                // detection so we never treat a dead refresh token as a
                // transient billing issue.
                if detect_auth_required(&error_msg) || structured_auth_required(&raw) {
                    let mut st = state.lock().unwrap();
                    st.pending_message_id = None;
                    st.state = ShimState::Idle;
                    st.state_changed_at = Instant::now();
                    let drain = drain_queue_errors(&mut st.message_queue, ShimState::Idle);
                    drop(st);

                    let _ = evt_channel.send(&Event::AuthRequired { message: error_msg });
                    for event in drain {
                        let _ = evt_channel.send(&event);
                    }
                    return;
                }

                // Detect quota/billing exhaustion — emit a specific event so
                // the daemon can pause dispatch and alert the human.
                let lower = error_msg.to_ascii_lowercase();
                if lower.contains("usage limit")
                    || lower.contains("quota")
                    || lower.contains("billing")
                    || lower.contains("purchase more credits")
                {
                    eprintln!("[shim-codex {shim_id}] QUOTA EXHAUSTED: {error_msg}");
                    if let Some(quota_block) = detect_quota_block(&error_msg) {
                        let mut st = state.lock().unwrap();
                        st.pending_message_id = None;
                        st.state = ShimState::Idle;
                        st.state_changed_at = Instant::now();
                        st.quota_blocked_until = quota_block.retry_at_epoch_secs;
                        let drain = drain_queue_errors(&mut st.message_queue, ShimState::Idle);
                        drop(st);

                        let _ = evt_channel.send(&Event::QuotaBlocked {
                            message: error_msg.clone(),
                            retry_at_epoch_secs: quota_block.retry_at_epoch_secs,
                            retry_at_label: quota_block.retry_at_label.clone(),
                        });
                        for event in drain {
                            let _ = evt_channel.send(&event);
                        }
                        return;
                    }
                }
            }

            "turn.completed" => {
                turn_completed = true;
                if let Some(usage) = evt.usage {
                    turn_usage.merge_snapshot(&usage);
                }
            }
            // turn.started and item.started are informational.
            _ => {}
        }
    }

    // Poll without holding the child mutex across a blocking wait, so shutdown
    // can terminate the process even if it closed stdout before exiting.
    let exit_status = loop {
        match child.lock().unwrap().try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(_) => break None,
            Ok(None) => {}
        }
        if state.lock().unwrap().state == ShimState::Dead {
            return;
        }
        thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS));
    };
    {
        let mut st = state.lock().unwrap();
        st.input_tokens = st
            .input_tokens
            .saturating_add(turn_usage.input_tokens.max(0) as u64);
        st.output_tokens = st
            .output_tokens
            .saturating_add(turn_usage.output_tokens.max(0) as u64);
        if st.state == ShimState::Dead {
            return;
        }
        if !exit_status.is_some_and(|status| status.success()) || !turn_completed {
            let message_id = st.pending_message_id.take();
            let drained = fail_queued_messages(&mut st.message_queue);
            st.state = ShimState::Idle;
            st.state_changed_at = Instant::now();
            drop(st);
            let _ = evt_channel.send(&Event::Error {
                command: "CodexExec".into(),
                reason: "Codex did not complete its turn successfully".into(),
            });
            if let Some(id) = message_id {
                let _ = evt_channel.send(&Event::DeliveryFailed {
                    id,
                    reason: "Codex turn failed".into(),
                });
            }
            for event in drained {
                let _ = evt_channel.send(&event);
            }
            let _ = evt_channel.send(&Event::StateChanged {
                from: ShimState::Working,
                to: ShimState::Idle,
                summary: "Codex turn failed".into(),
            });
            return;
        }
        let _ = evt_channel.send(&Event::SessionStats {
            output_bytes: st.cumulative_output_bytes,
            uptime_secs: st.started_at.elapsed().as_secs(),
            input_tokens: st.input_tokens,
            output_tokens: st.output_tokens,
            context_usage_pct: None,
        });
    }

    // Transition Working → Idle, emit Completion
    let mut st = state.lock().unwrap();
    if st.state == ShimState::Dead {
        return;
    }
    let response = std::mem::take(&mut st.accumulated_response);
    let last_lines = last_n_lines_of(&response, 5);
    let msg_id = st.pending_message_id.take();
    st.state = ShimState::Idle;
    st.state_changed_at = Instant::now();

    // Check for queued messages
    let queued_msg = if !st.message_queue.is_empty() {
        st.message_queue.pop_front()
    } else {
        None
    };

    if let Some(ref qm) = queued_msg {
        st.pending_message_id = qm.message_id.clone();
        st.state = ShimState::Working;
        st.state_changed_at = Instant::now();
        st.accumulated_response.clear();
    }

    let thread_id = st.thread_id.clone();
    let program = st.program.clone();
    let cwd_owned = st.cwd.clone();
    let next_text = queued_msg.as_ref().map(|message| {
        st.launch_context.prompt(
            st.thread_id.is_none(),
            &format_injected_message(&message.from, &message.body),
        )
    });
    let queue_depth = st.message_queue.len();
    drop(st);

    let _ = evt_channel.send(&Event::StateChanged {
        from: ShimState::Working,
        to: ShimState::Idle,
        summary: last_lines.clone(),
    });
    let _ = evt_channel.send(&Event::Completion {
        message_id: msg_id,
        response,
        last_lines,
    });

    // Drain queued message by spawning another codex exec
    if let Some(qm) = queued_msg {
        let _ = evt_channel.send(&Event::StateChanged {
            from: ShimState::Idle,
            to: ShimState::Working,
            summary: format!("delivering queued message ({queue_depth} remaining)"),
        });

        let text = next_text.unwrap_or_else(|| format_injected_message(&qm.from, &qm.body));
        let (exec_program, exec_args) = codex_types::codex_sdk_args(&program, thread_id.as_deref());

        // Recursive call for queued message (same thread)
        run_codex_exec(
            shim_id,
            &exec_program,
            &exec_args,
            &text,
            &cwd_owned,
            state,
            evt_channel,
            pty_log,
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QuotaBlock {
    retry_at_epoch_secs: Option<u64>,
    retry_at_label: Option<String>,
}

/// Structured codes win over legacy text matching. Unknown structured codes
/// are left to the existing transient/auth/quota handlers, not guessed terminal.
fn model_configuration_error(raw: &serde_json::Value, message: &str) -> Option<Option<String>> {
    let code = raw
        .pointer("/error/code")
        .or_else(|| raw.get("code"))
        .or_else(|| raw.pointer("/error/type"))
        .and_then(|v| v.as_str());
    if let Some(code) = code {
        return matches!(
            code.to_ascii_lowercase().as_str(),
            "model_not_found" | "unsupported_model" | "invalid_model" | "model_not_supported"
        )
        .then(|| Some(code.to_string()));
    }
    let lower = message.to_ascii_lowercase();
    (lower.contains("model_not_found")
        || lower.contains("unsupported model")
        || lower.contains("not supported when using codex")
        || lower.contains("invalid model"))
    .then_some(None)
}

fn structured_auth_required(raw: &serde_json::Value) -> bool {
    raw.pointer("/error/code")
        .or_else(|| raw.get("code"))
        .or_else(|| raw.pointer("/error/type"))
        .and_then(|v| v.as_str())
        .is_some_and(|code| {
            matches!(
                code.to_ascii_lowercase().as_str(),
                "invalid_api_key"
                    | "authentication_error"
                    | "unauthorized"
                    | "refresh_token_reused"
            )
        })
}

/// Detect the non-recoverable codex auth-credential error. The refresh token
/// can only be used once; once that single use has been consumed (typically by
/// another concurrent codex process), no amount of respawning can recover —
/// a human has to `codex login` again. Keep the matcher narrow so that
/// transient network-level auth failures still flow through the normal retry
/// paths.
fn detect_auth_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("refresh token was already used")
        || (lower.contains("failed to refresh token") && lower.contains("log out and sign in"))
}

#[cfg(test)]
mod detect_auth_required_tests {
    use super::detect_auth_required;

    #[test]
    fn matches_refresh_token_already_used() {
        assert!(detect_auth_required(
            "Your access token could not be refreshed because your refresh token was already used. \
             Please log out and sign in again."
        ));
    }

    #[test]
    fn matches_generic_failed_to_refresh_with_login_hint() {
        assert!(detect_auth_required(
            "Failed to refresh token: please log out and sign in again"
        ));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(!detect_auth_required("stream error"));
        assert!(!detect_auth_required("usage limit reached"));
        assert!(!detect_auth_required("context length exceeded"));
    }
}

fn detect_quota_block(message: &str) -> Option<QuotaBlock> {
    let lower = message.to_ascii_lowercase();
    let quota_hit = lower.contains("usage limit")
        || lower.contains("purchase more credits")
        || (lower.contains("quota") && lower.contains("try again"));
    if !quota_hit {
        return None;
    }

    let retry_at_epoch_secs = message
        .split_once("try again at")
        .and_then(|(_, tail)| parse_quota_retry_at(tail.trim()));
    let retry_at_label = retry_at_epoch_secs.map(format_quota_retry_label);
    Some(QuotaBlock {
        retry_at_epoch_secs,
        retry_at_label,
    })
}

fn parse_quota_retry_at(value: &str) -> Option<u64> {
    let ordinal_suffixes = Regex::new(r"(?i)\b(\d{1,2})(st|nd|rd|th)\b").ok()?;
    let normalized = ordinal_suffixes
        .replace_all(value.trim().trim_end_matches('.'), "$1")
        .to_string();
    let naive = chrono::NaiveDateTime::parse_from_str(&normalized, "%b %e, %Y %I:%M %p")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&normalized, "%B %e, %Y %I:%M %p"))
        .ok()?;
    let local = chrono::Local
        .from_local_datetime(&naive)
        .single()
        .or_else(|| chrono::Local.from_local_datetime(&naive).earliest())
        .or_else(|| chrono::Local.from_local_datetime(&naive).latest())?;
    Some(local.timestamp().max(0) as u64)
}

fn format_quota_retry_label(unix_secs: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};

    let dt = UNIX_EPOCH + Duration::from_secs(unix_secs);
    let datetime: chrono::DateTime<chrono::Local> = dt.into();
    datetime.format("%Y-%m-%d %H:%M").to_string()
}

fn current_unix_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fail_queued_messages(queue: &mut VecDeque<QueuedMessage>) -> Vec<Event> {
    queue
        .drain(..)
        .map(|message| match message.message_id {
            Some(id) => Event::DeliveryFailed {
                id,
                reason: "Codex execution failed before queued delivery".into(),
            },
            None => Event::Error {
                command: "SendMessage".into(),
                reason: "Codex execution failed before queued delivery".into(),
            },
        })
        .collect()
}

struct CodexExecGuard {
    child: Arc<Mutex<Child>>,
    state: Arc<Mutex<CodexState>>,
}
impl Drop for CodexExecGuard {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap();
        if st
            .active_child
            .as_ref()
            .is_some_and(|child| Arc::ptr_eq(child, &self.child))
        {
            st.active_child = None;
        }
        drop(st);
        terminate_child(&mut self.child.lock().unwrap());
    }
}
fn stop_codex_exec(state: &Arc<Mutex<CodexState>>) {
    let child = {
        let mut st = state.lock().unwrap();
        st.state = ShimState::Dead;
        st.state_changed_at = Instant::now();
        st.message_queue.clear();
        st.active_child.take()
    };
    if let Some(child) = child {
        terminate_child(&mut child.lock().unwrap());
    }
}

/// Terminate a child process: SIGTERM, grace period, then SIGKILL.
#[allow(dead_code)]
fn terminate_child(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let pid = child.id();
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(GROUP_TERM_GRACE_SECS);
        loop {
            if Instant::now() > deadline {
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => return,
                _ => thread::sleep(Duration::from_millis(PROCESS_EXIT_POLL_MS)),
            }
        }
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[allow(unreachable_code)]
    {
        let _ = child.kill();
    }
}

/// Extract the last N lines from a string.
fn last_n_lines_of(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::protocol;

    #[test]
    fn last_n_lines_basic() {
        assert_eq!(last_n_lines_of("a\nb\nc", 2), "b\nc");
        assert_eq!(last_n_lines_of("a\nb\nc", 10), "a\nb\nc");
        assert_eq!(last_n_lines_of("", 5), "");
    }

    #[test]
    fn codex_state_initial() {
        let st = CodexState {
            state: ShimState::Idle,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            thread_id: None,
            launch_context: Default::default(),
            accumulated_response: String::new(),
            pending_message_id: None,
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            input_tokens: 0,
            output_tokens: 0,
            active_child: None,
            program: "codex".into(),
            cwd: std::path::PathBuf::from("/tmp"),
            context_approaching_emitted: false,
            quota_blocked_until: None,
            configuration_error: None,
        };
        assert_eq!(st.state, ShimState::Idle);
        assert!(st.thread_id.is_none());
    }

    #[test]
    fn channel_events_roundtrip() {
        let (parent_sock, child_sock) = protocol::socketpair().unwrap();
        let mut parent = protocol::Channel::new(parent_sock);
        let mut child = protocol::Channel::new(child_sock);

        child.send(&Event::Ready).unwrap();
        let event: Event = parent.recv().unwrap().unwrap();
        assert!(matches!(event, Event::Ready));
    }

    #[test]
    fn parse_quota_retry_at_handles_codex_usage_limit_timestamp() {
        let parsed = parse_quota_retry_at("Apr 16th, 2026 12:54 PM.");
        assert!(parsed.is_some(), "expected retry timestamp to parse");
    }

    #[test]
    fn detect_quota_block_extracts_retry_metadata() {
        let quota = detect_quota_block(
            "You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Apr 16th, 2026 12:54 PM.",
        )
        .expect("expected quota block");
        assert!(quota.retry_at_epoch_secs.is_some());
        assert_eq!(quota.retry_at_label.as_deref(), Some("2026-04-16 12:54"));
    }

    #[test]
    fn detect_quota_block_ignores_non_quota_failures() {
        assert!(detect_quota_block("stream error").is_none());
    }
    #[test]
    fn configuration_errors_use_codes_before_legacy_text() {
        for event_type in ["turn.failed", "error"] {
            let event = serde_json::json!({"type":event_type,"error":{"code":"model_not_found","message":"unavailable"}});
            assert_eq!(
                model_configuration_error(&event, "unavailable"),
                Some(Some("model_not_found".into()))
            );
        }
        assert_eq!(
            model_configuration_error(
                &serde_json::json!({}),
                "Model is not supported when using Codex"
            ),
            Some(None)
        );
        assert!(
            model_configuration_error(
                &serde_json::json!({"error":{"code":"rate_limit_exceeded"}}),
                "unsupported model"
            )
            .is_none()
        );
        assert!(structured_auth_required(
            &serde_json::json!({"error":{"code":"invalid_api_key"}})
        ));
        assert!(!structured_auth_required(
            &serde_json::json!({"error":{"code":"model_not_found"}})
        ));
        assert!(
            model_configuration_error(
                &serde_json::json!({"error":{"code":"invalid_api_key"}}),
                "invalid credentials"
            )
            .is_none()
        );
        assert!(model_configuration_error(&serde_json::json!({}), "stream error").is_none());
        assert!(
            model_configuration_error(&serde_json::json!({}), "refresh token was already used")
                .is_none()
        );
    }
    #[test]
    fn fake_codex_exec_reports_and_persists_actual_thread() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let program = tmp.path().join("fake-codex");
        std::fs::write(&program,format!("#!/bin/sh\n/bin/cat > /dev/null\nprintf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"{id}\"}}' '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"m1\",\"type\":\"agent_message\",\"text\":\"done\"}}}}' '{{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":10,\"cached_input_tokens\":8,\"output_tokens\":3}}}}'\n")).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = Arc::new(Mutex::new(CodexState {
            state: ShimState::Working,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            thread_id: None,
            launch_context: Default::default(),
            accumulated_response: String::new(),
            pending_message_id: Some("message-1".into()),
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            input_tokens: 0,
            output_tokens: 0,
            active_child: None,
            program: program.to_string_lossy().into(),
            cwd: tmp.path().into(),
            context_approaching_emitted: false,
            quota_blocked_until: None,
            configuration_error: None,
        }));
        let (parent, child) = protocol::socketpair().unwrap();
        let mut parent = protocol::Channel::new(parent);
        parent
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut child = protocol::Channel::new(child);
        run_codex_exec(
            "worker",
            program.to_str().unwrap(),
            &[],
            "assigned work",
            tmp.path(),
            &state,
            &mut child,
            None,
        );
        assert_eq!(state.lock().unwrap().input_tokens, 10);
        assert_eq!(state.lock().unwrap().output_tokens, 3);
        assert!(state.lock().unwrap().active_child.is_none());
        assert_eq!(
            state.lock().unwrap().thread_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(
            super::super::codex_session::LaunchContext::load(tmp.path(), "worker")
                .unwrap()
                .resume_session_id
                .as_deref(),
            Some(id.as_str())
        );
        assert!(
            matches!(parent.recv::<Event>().unwrap().unwrap(),Event::SessionStarted{session_id} if session_id==id)
        );
    }
    fn fake_working_state(cwd: &std::path::Path) -> Arc<Mutex<CodexState>> {
        Arc::new(Mutex::new(CodexState {
            state: ShimState::Working,
            state_changed_at: Instant::now(),
            started_at: Instant::now(),
            thread_id: None,
            launch_context: Default::default(),
            accumulated_response: String::new(),
            pending_message_id: Some("message-1".into()),
            message_queue: VecDeque::new(),
            cumulative_output_bytes: 0,
            input_tokens: 0,
            output_tokens: 0,
            active_child: None,
            program: "/bin/false".into(),
            cwd: cwd.into(),
            context_approaching_emitted: false,
            quota_blocked_until: None,
            configuration_error: None,
        }))
    }

    #[test]
    fn failed_codex_process_is_not_reported_as_a_completed_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_working_state(tmp.path());
        state
            .lock()
            .unwrap()
            .message_queue
            .push_back(QueuedMessage {
                from: "manager".into(),
                body: "queued".into(),
                message_id: Some("queued-1".into()),
            });
        let (parent, child) = protocol::socketpair().unwrap();
        let mut parent = protocol::Channel::new(parent);
        parent
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut child = protocol::Channel::new(child);
        run_codex_exec(
            "worker",
            "/bin/sh",
            &["-c".into(), "exit 1".into()],
            "task",
            tmp.path(),
            &state,
            &mut child,
            None,
        );
        let mut failed_ids = Vec::new();
        for _ in 0..4 {
            let event = parent.recv::<Event>().unwrap().unwrap();
            assert!(!matches!(event, Event::Completion { .. }));
            if let Event::DeliveryFailed { id, .. } = event {
                failed_ids.push(id);
            }
        }
        failed_ids.sort();
        assert_eq!(failed_ids, vec!["message-1", "queued-1"]);
        assert!(state.lock().unwrap().active_child.is_none());
    }

    #[test]
    fn shutdown_terminates_the_owned_exec_and_discards_queued_work() {
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let state = fake_working_state(tmp.path());
        let child = Arc::new(Mutex::new(
            Command::new("/bin/sleep")
                .arg("30")
                .process_group(0)
                .spawn()
                .unwrap(),
        ));
        {
            let mut st = state.lock().unwrap();
            st.active_child = Some(Arc::clone(&child));
            st.message_queue.push_back(QueuedMessage {
                from: "manager".into(),
                body: "queued".into(),
                message_id: None,
            });
        }
        let started = Instant::now();
        stop_codex_exec(&state);
        assert!(started.elapsed() < Duration::from_secs(4));
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
        let st = state.lock().unwrap();
        assert_eq!(st.state, ShimState::Dead);
        assert!(st.message_queue.is_empty());
        assert!(st.active_child.is_none());
    }
}
