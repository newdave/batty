//! Shared utilities used by both PTY runtime and SDK runtime.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::protocol::{Channel, Event, ShimState};
use super::pty_log::PtyLogWriter;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of messages that can be queued while the agent is working.
pub const MAX_QUEUE_DEPTH: usize = 16;

/// How often to report session stats (secs).
pub const SESSION_STATS_INTERVAL_SECS: u64 = 10;

/// How often to poll a child process for exit (millis).
pub const PROCESS_EXIT_POLL_MS: u64 = 100;

/// Grace period between SIGTERM and SIGKILL when stopping an agent group (secs).
pub const GROUP_TERM_GRACE_SECS: u64 = 2;

/// Maximum number of automatic fix-and-retest loops before escalation.
pub const TEST_FAILURE_MAX_ITERATIONS: u8 = 5;

// ---------------------------------------------------------------------------
// Log lines
// ---------------------------------------------------------------------------

/// Write one complete log line (newline included) to stderr with a single
/// `write_all`. `eprintln!` issues a separate write per format piece, so lines
/// from several shim processes sharing the daemon log fd interleave mid-line.
pub fn write_log_line(args: std::fmt::Arguments<'_>) {
    use std::io::Write;
    let line = format_log_line(args);
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

fn format_log_line(args: std::fmt::Arguments<'_>) -> String {
    let mut line = std::fmt::format(args);
    line.push('\n');
    line
}

#[cfg(test)]
mod log_line_tests {
    use super::format_log_line;

    #[test]
    fn log_line_is_single_buffer_with_trailing_newline() {
        let line = format_log_line(format_args!("[shim-codex {}] stderr: {}", "e1", "boom"));
        assert_eq!(line, "[shim-codex e1] stderr: boom\n");
    }
}

/// Drop-in replacement for `eprintln!` that emits each line atomically.
macro_rules! shim_eprintln {
    ($($arg:tt)*) => {
        $crate::shim::common::write_log_line(format_args!($($arg)*))
    };
}
pub(crate) use shim_eprintln;

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

pub fn reply_target_for(sender: &str) -> &str {
    sender
}

pub fn format_injected_message(sender: &str, body: &str) -> String {
    let reply_to = reply_target_for(sender);
    format!(
        "--- Message from {sender} ---\n\
         Reply-To: {reply_to}\n\
         If you need to reply, use: batty send {reply_to} \"<your reply>\"\n\
         \n\
         {body}"
    )
}

/// Extract the last N lines from a string.
pub fn last_n_lines_of(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// Queued message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub from: String,
    pub body: String,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestFailureFollowup {
    pub body: String,
    pub notice: String,
    pub next_iteration_count: u8,
    pub escalate: bool,
}

// ---------------------------------------------------------------------------
// Queue drain helper
// ---------------------------------------------------------------------------

/// Drain all queued messages, emitting an Error event for each one.
/// Used when the agent enters a terminal state (Dead, ContextExhausted).
pub fn drain_queue_errors(
    queue: &mut VecDeque<QueuedMessage>,
    terminal_state: ShimState,
) -> Vec<Event> {
    let mut events = Vec::new();
    while let Some(msg) = queue.pop_front() {
        events.push(Event::Error {
            command: "SendMessage".into(),
            reason: format!(
                "agent entered {} state, queued message dropped{}",
                terminal_state,
                msg.message_id
                    .map(|id| format!(" (id: {id})"))
                    .unwrap_or_default(),
            ),
        });
    }
    events
}

/// The error reported when a queued message (or a shim-generated follow-up)
/// could not be written to the agent's stdin after a turn ended.
pub fn queued_write_failed_event(message_id: Option<&str>, detail: &str) -> Event {
    Event::Error {
        command: "SendMessage".into(),
        reason: format!(
            "stdin write failed for queued message{}: {detail}",
            message_id
                .map(|id| format!(" (id: {id})"))
                .unwrap_or_default(),
        ),
    }
}

/// Handle a `SendMessage` that arrived while the agent is not Idle.
///
/// Working: queue the message (dropping the oldest at `MAX_QUEUE_DEPTH`) and
/// return the events to send, in order. Any other state: leave the queue
/// alone and return the "cannot accept message" Error.
///
/// Pure state mutation; callers send the events (after releasing their state
/// lock, except where they deliberately keep it).
pub fn enqueue_or_reject(
    queue: &mut VecDeque<QueuedMessage>,
    state: ShimState,
    msg: QueuedMessage,
) -> Vec<Event> {
    if state != ShimState::Working {
        return vec![Event::Error {
            command: "SendMessage".into(),
            reason: format!("agent in {state} state, cannot accept message"),
        }];
    }

    let mut events = Vec::new();
    if queue.len() >= MAX_QUEUE_DEPTH {
        let dropped = queue.pop_front();
        let dropped_id = dropped.as_ref().and_then(|m| m.message_id.clone());
        events.push(Event::Error {
            command: "SendMessage".into(),
            reason: format!(
                "message queue full ({MAX_QUEUE_DEPTH}), dropped oldest message{}",
                dropped_id
                    .map(|id| format!(" (id: {id})"))
                    .unwrap_or_default(),
            ),
        });
    }
    queue.push_back(msg);
    events.push(Event::Warning {
        message: format!(
            "message queued while agent working (depth: {})",
            queue.len()
        ),
        idle_secs: None,
    });
    events
}

// ---------------------------------------------------------------------------
// PTY log
// ---------------------------------------------------------------------------

/// Open the optional PTY log writer shared by the output reader and the
/// command loop. `None` path means logging is disabled.
pub fn open_pty_log(path: Option<&Path>) -> Result<Option<Arc<Mutex<PtyLogWriter>>>> {
    path.map(|p| PtyLogWriter::new(p).context("failed to create PTY log"))
        .transpose()
        .map(|w| w.map(|w| Arc::new(Mutex::new(w))))
}

// ---------------------------------------------------------------------------
// Session stats
// ---------------------------------------------------------------------------

/// Spawn the periodic session-stats reporter. Every
/// `SESSION_STATS_INTERVAL_SECS` the thread calls `snapshot`, which should take
/// and release its own state lock and return the event to send, or `None` to
/// stop (e.g. the agent is Dead). The thread also stops if the send fails.
pub fn spawn_session_stats<F>(channel: Channel, snapshot: F) -> JoinHandle<()>
where
    F: FnMut() -> Option<Event> + Send + 'static,
{
    spawn_session_stats_every(
        channel,
        Duration::from_secs(SESSION_STATS_INTERVAL_SECS),
        snapshot,
    )
}

fn spawn_session_stats_every<F>(
    mut channel: Channel,
    interval: Duration,
    mut snapshot: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Option<Event> + Send + 'static,
{
    thread::spawn(move || {
        loop {
            thread::sleep(interval);
            let Some(event) = snapshot() else {
                return;
            };
            if channel.send(&event).is_err() {
                return;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Command replies for non-PTY runtimes
// ---------------------------------------------------------------------------

/// Reply to `CaptureScreen` for runtimes without a terminal: the accumulated
/// response (or its last `last_n_lines` lines) with the cursor at the origin.
pub fn screen_capture_event(accumulated_response: &str, last_n_lines: Option<usize>) -> Event {
    let content = match last_n_lines {
        Some(n) => last_n_lines_of(accumulated_response, n),
        None => accumulated_response.to_string(),
    };
    Event::ScreenCapture {
        content,
        cursor_row: 0,
        cursor_col: 0,
    }
}

/// Reply to `GetState`: the current state and whole seconds since it changed.
pub fn state_event(state: ShimState, state_changed_at: Instant) -> Event {
    Event::State {
        state,
        since_secs: state_changed_at.elapsed().as_secs(),
    }
}

// ---------------------------------------------------------------------------
// Context exhaustion detection
// ---------------------------------------------------------------------------

const EXHAUSTION_PATTERNS: &[&str] = &[
    "context window exceeded",
    "context window is full",
    "conversation is too long",
    "maximum context length",
    "context limit reached",
    "truncated due to context limit",
    "input exceeds the model",
    "prompt is too long",
];

/// Check if text contains known context exhaustion phrases (hard failure).
pub fn detect_context_exhausted(text: &str) -> bool {
    let lower = text.to_lowercase();
    EXHAUSTION_PATTERNS.iter().any(|p| lower.contains(p))
}

// ---------------------------------------------------------------------------
// Context approaching-limit detection (early warning)
// ---------------------------------------------------------------------------

const CONTEXT_APPROACHING_PATTERNS: &[&str] = &[
    "automatically compress prior messages",
    "context window is approaching",
    "approaching context limit",
    "context is getting large",
    "conversation history has been compressed",
    "messages were compressed",
    "running low on context",
    "nearing the context limit",
];

/// Check if text contains signals that the context window is under pressure
/// but not yet fully exhausted. Returns true for early-warning patterns
/// (e.g. automatic compression notifications from Claude).
pub fn detect_context_approaching_limit(text: &str) -> bool {
    let lower = text.to_lowercase();
    CONTEXT_APPROACHING_PATTERNS
        .iter()
        .any(|p| lower.contains(p))
}

const TEST_COMMAND_PATTERNS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "pytest",
    "npm test",
    "pnpm test",
    "yarn test",
    "go test",
    "bundle exec rspec",
    "mix test",
    "test result:",
];

const TEST_FAILURE_PATTERNS: &[&str] = &[
    "test result: failed",
    "error: test failed",
    "failing tests:",
    "failures:",
    "failures (",
    "test failed, to rerun pass",
];

/// Check if text looks like a real test runner failure instead of a prose mention.
pub fn detect_test_failure(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has_test_context = TEST_COMMAND_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern));
    if !has_test_context {
        return false;
    }

    TEST_FAILURE_PATTERNS
        .iter()
        .any(|pattern| lower.contains(pattern))
        || lower
            .lines()
            .map(str::trim)
            .any(|line| line.starts_with("test ") && line.ends_with("... failed"))
}

pub fn detect_test_failure_followup(
    text: &str,
    iteration_count: u8,
) -> Option<TestFailureFollowup> {
    if !detect_test_failure(text) {
        return None;
    }

    if iteration_count < TEST_FAILURE_MAX_ITERATIONS {
        let attempt = iteration_count + 1;
        return Some(TestFailureFollowup {
            body: format!(
                "tests failed — fix and retest before reporting completion.\n\
                 Attempt {attempt}/{TEST_FAILURE_MAX_ITERATIONS}.\n\
                 Re-run cargo test after fixing the failures.\n\
                 Do not send a completion packet unless tests_passed=true."
            ),
            notice: format!(
                "tests failed — fix and retest (attempt {attempt}/{TEST_FAILURE_MAX_ITERATIONS})"
            ),
            next_iteration_count: attempt,
            escalate: false,
        });
    }

    Some(TestFailureFollowup {
        body: format!(
            "tests failed — fix and retest loop exhausted after \
             {TEST_FAILURE_MAX_ITERATIONS} attempts.\n\
             Stop reporting completion, send a blocker or escalation with the failing \
             test summary, and wait for direction."
        ),
        notice: format!(
            "tests failed repeatedly — escalation required after \
             {TEST_FAILURE_MAX_ITERATIONS} attempts"
        ),
        next_iteration_count: TEST_FAILURE_MAX_ITERATIONS,
        escalate: true,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_n_lines_basic() {
        let text = "a\nb\nc\nd\ne";
        assert_eq!(last_n_lines_of(text, 3), "c\nd\ne");
        assert_eq!(last_n_lines_of(text, 10), "a\nb\nc\nd\ne");
        assert_eq!(last_n_lines_of(text, 0), "");
    }

    #[test]
    fn last_n_lines_empty() {
        assert_eq!(last_n_lines_of("", 5), "");
    }

    #[test]
    fn format_message_includes_sender_and_body() {
        let msg = format_injected_message("manager", "Fix the bug");
        assert!(msg.contains("Message from manager"));
        assert!(msg.contains("Reply-To: manager"));
        assert!(msg.contains("Fix the bug"));
        assert!(msg.contains("batty send manager"));
        assert!(msg.starts_with("--- Message from manager ---"));
        assert!(msg.ends_with("Fix the bug"));
    }

    #[test]
    fn reply_target_is_identity() {
        assert_eq!(reply_target_for("architect"), "architect");
        assert_eq!(reply_target_for("eng-1-1"), "eng-1-1");
    }

    #[test]
    fn drain_queue_errors_empties_queue() {
        let mut queue = VecDeque::new();
        queue.push_back(QueuedMessage {
            from: "mgr".into(),
            body: "task 1".into(),
            message_id: Some("id-1".into()),
        });
        queue.push_back(QueuedMessage {
            from: "mgr".into(),
            body: "task 2".into(),
            message_id: None,
        });
        let events = drain_queue_errors(&mut queue, ShimState::Dead);
        assert_eq!(events.len(), 2);
        assert!(queue.is_empty());

        match &events[0] {
            Event::Error { reason, .. } => {
                assert!(reason.contains("dead"));
                assert!(reason.contains("id-1"));
            }
            _ => panic!("expected Error event"),
        }
    }

    #[test]
    fn queued_write_failed_event_names_the_message_when_known() {
        let Event::Error { command, reason } = queued_write_failed_event(Some("q7"), "Broken pipe")
        else {
            panic!("expected Error");
        };
        assert_eq!(command, "SendMessage");
        assert_eq!(
            reason,
            "stdin write failed for queued message (id: q7): Broken pipe"
        );
        let Event::Error { reason, .. } = queued_write_failed_event(None, "closed") else {
            panic!("expected Error");
        };
        assert_eq!(reason, "stdin write failed for queued message: closed");
    }

    #[test]
    fn drain_queue_errors_omits_id_suffix_and_names_terminal_state() {
        let mut queue = VecDeque::new();
        queue.push_back(QueuedMessage {
            from: "u".into(),
            body: "b".into(),
            message_id: None,
        });
        let events = drain_queue_errors(&mut queue, ShimState::ContextExhausted);
        match &events[0] {
            Event::Error { reason, .. } => {
                assert!(reason.contains("context_exhausted"));
                assert!(!reason.contains("(id:"));
            }
            _ => panic!("expected Error event"),
        }
    }

    #[test]
    fn drain_queue_empty_is_noop() {
        let mut queue = VecDeque::new();
        let events = drain_queue_errors(&mut queue, ShimState::Dead);
        assert!(events.is_empty());
    }

    #[test]
    fn context_exhaustion_detected() {
        assert!(detect_context_exhausted("Error: context window exceeded"));
        assert!(detect_context_exhausted(
            "The CONVERSATION IS TOO LONG to continue"
        ));
        assert!(detect_context_exhausted("maximum context length reached"));
    }

    #[test]
    fn context_exhaustion_not_detected_for_normal_text() {
        assert!(!detect_context_exhausted("Writing function to parse YAML"));
        assert!(!detect_context_exhausted("context manager initialized"));
    }

    #[test]
    fn context_approaching_limit_detected() {
        assert!(detect_context_approaching_limit(
            "The system will automatically compress prior messages in your conversation"
        ));
        assert!(detect_context_approaching_limit(
            "conversation history has been compressed to save space"
        ));
        assert!(detect_context_approaching_limit(
            "Context window is approaching its maximum capacity"
        ));
    }

    #[test]
    fn context_approaching_not_detected_for_normal_text() {
        assert!(!detect_context_approaching_limit(
            "context manager initialized"
        ));
        assert!(!detect_context_approaching_limit(
            "compression algorithm works"
        ));
    }

    #[test]
    fn test_failure_detected_for_cargo_test_output() {
        let output = "running 2 tests\n\
                      test foo::bar ... FAILED\n\
                      failures:\n\
                      test result: FAILED. 1 passed; 1 failed; 0 ignored;";
        assert!(detect_test_failure(output));
    }

    #[test]
    fn test_failure_not_detected_for_prompt_text_only() {
        assert!(!detect_test_failure(
            "tests failed — fix and retest before reporting completion"
        ));
    }

    #[test]
    fn test_failure_followup_retries_before_escalating() {
        let output = "cargo test\nfailures:\ntest result: FAILED.";
        let followup = detect_test_failure_followup(output, 0).expect("followup");
        assert!(!followup.escalate);
        assert_eq!(followup.next_iteration_count, 1);
        assert!(followup.notice.contains("attempt 1/5"));

        let escalation =
            detect_test_failure_followup(output, TEST_FAILURE_MAX_ITERATIONS).expect("escalation");
        assert!(escalation.escalate);
        assert_eq!(escalation.next_iteration_count, TEST_FAILURE_MAX_ITERATIONS);
        assert!(escalation.notice.contains("escalation required"));
    }

    fn queued(id: Option<&str>) -> QueuedMessage {
        QueuedMessage {
            from: "user".into(),
            body: "body".into(),
            message_id: id.map(String::from),
        }
    }

    #[test]
    fn enqueue_under_cap_queues_and_warns() {
        let mut queue = VecDeque::new();
        let events = enqueue_or_reject(&mut queue, ShimState::Working, queued(Some("a")));
        assert_eq!(queue.len(), 1);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::Warning { message, idle_secs } => {
                assert_eq!(message, "message queued while agent working (depth: 1)");
                assert!(idle_secs.is_none());
            }
            other => panic!("expected Warning, got {other:?}"),
        }
    }

    #[test]
    fn enqueue_at_cap_drops_oldest_with_id_in_reason() {
        let mut queue = VecDeque::new();
        for i in 0..MAX_QUEUE_DEPTH {
            queue.push_back(queued(Some(&format!("m{i}"))));
        }
        let events = enqueue_or_reject(&mut queue, ShimState::Working, queued(Some("new")));
        assert_eq!(queue.len(), MAX_QUEUE_DEPTH);
        assert_eq!(queue.front().unwrap().message_id.as_deref(), Some("m1"));
        assert_eq!(queue.back().unwrap().message_id.as_deref(), Some("new"));
        assert_eq!(events.len(), 2);
        match &events[0] {
            Event::Error { command, reason } => {
                assert_eq!(command, "SendMessage");
                assert_eq!(
                    reason,
                    "message queue full (16), dropped oldest message (id: m0)"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
        assert!(matches!(
            &events[1],
            Event::Warning { message, .. }
                if message == "message queued while agent working (depth: 16)"
        ));
    }

    #[test]
    fn enqueue_at_cap_without_dropped_id_omits_suffix() {
        let mut queue = VecDeque::new();
        for _ in 0..MAX_QUEUE_DEPTH {
            queue.push_back(queued(None));
        }
        let events = enqueue_or_reject(&mut queue, ShimState::Working, queued(None));
        assert!(matches!(
            &events[0],
            Event::Error { reason, .. }
                if reason == "message queue full (16), dropped oldest message"
        ));
    }

    #[test]
    fn screen_capture_event_none_returns_whole_response() {
        match screen_capture_event("a\nb\nc", None) {
            Event::ScreenCapture {
                content,
                cursor_row,
                cursor_col,
            } => {
                assert_eq!(content, "a\nb\nc");
                assert_eq!((cursor_row, cursor_col), (0, 0));
            }
            other => panic!("expected ScreenCapture, got {other:?}"),
        }
    }

    #[test]
    fn screen_capture_event_some_returns_last_lines() {
        match screen_capture_event("a\nb\nc", Some(2)) {
            Event::ScreenCapture {
                content,
                cursor_row,
                cursor_col,
            } => {
                assert_eq!(content, "b\nc");
                assert_eq!((cursor_row, cursor_col), (0, 0));
            }
            other => panic!("expected ScreenCapture, got {other:?}"),
        }
    }

    #[test]
    fn state_event_reports_state_and_elapsed_secs() {
        let changed = Instant::now() - std::time::Duration::from_secs(5);
        match state_event(ShimState::Working, changed) {
            Event::State { state, since_secs } => {
                assert_eq!(state, ShimState::Working);
                assert!((5..=6).contains(&since_secs), "since_secs={since_secs}");
            }
            other => panic!("expected State, got {other:?}"),
        }
    }

    #[test]
    fn session_stats_thread_exits_when_snapshot_returns_none() {
        let (parent, child) = super::super::protocol::socketpair().unwrap();
        let mut parent = Channel::new(parent);
        parent
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut calls = 0;
        let handle =
            spawn_session_stats_every(Channel::new(child), Duration::from_millis(10), move || {
                calls += 1;
                (calls <= 2).then_some(Event::SessionStats {
                    output_bytes: calls,
                    uptime_secs: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    context_usage_pct: None,
                })
            });
        handle.join().unwrap();
        for expected in 1..=2 {
            match parent.recv::<Event>().unwrap() {
                Some(Event::SessionStats { output_bytes, .. }) => {
                    assert_eq!(output_bytes, expected)
                }
                other => panic!("expected SessionStats, got {other:?}"),
            }
        }
        // Thread (and its channel) is gone: no third event, just EOF.
        assert!(parent.recv::<Event>().unwrap().is_none());
    }

    #[test]
    fn open_pty_log_none_path_disables_logging() {
        assert!(open_pty_log(None).unwrap().is_none());
    }

    #[test]
    fn open_pty_log_creates_file_and_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sub").join("agent.pty.log");
        let log = open_pty_log(Some(&path)).unwrap();
        assert!(log.is_some());
        assert!(path.exists());
    }

    #[test]
    fn open_pty_log_error_has_context() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let err = open_pty_log(Some(&blocker.join("agent.pty.log")))
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "failed to create PTY log");
    }

    #[test]
    fn enqueue_in_non_working_state_rejects_without_queueing() {
        let mut queue = VecDeque::new();
        let events = enqueue_or_reject(&mut queue, ShimState::Dead, queued(Some("a")));
        assert!(queue.is_empty());
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            Event::Error { command, reason }
                if command == "SendMessage"
                    && reason == "agent in dead state, cannot accept message"
        ));
    }
}
