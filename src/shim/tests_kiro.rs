//! Integration tests for the Kiro ACP shim runtime.
//!
//! These tests spawn mock subprocesses that simulate the Kiro ACP JSON-RPC
//! protocol, exercising the runtime's initialization handshake, message
//! delivery, streaming updates, and permission auto-approval.
//!
//! Gated behind `shim-integration` feature flag.
//! Run with: cargo test --features shim-integration tests_kiro

use std::path::PathBuf;
use std::time::Duration;

use crate::shim::protocol::{self, Channel, Command, Event, ShimState};
use crate::shim::runtime::ShimArgs;

/// Check if an error is a read timeout (WouldBlock/TimedOut).
fn is_timeout_error(e: &anyhow::Error) -> bool {
    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )
    } else {
        false
    }
}

/// Wait for a specific event type, skipping others, with a timeout.
fn wait_for_event<F>(ch: &mut Channel, timeout: Duration, predicate: F) -> Option<Event>
where
    F: Fn(&Event) -> bool,
{
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }
        match ch.recv::<Event>() {
            Ok(Some(evt)) if predicate(&evt) => return Some(evt),
            Ok(Some(_)) => continue,
            Ok(None) => return None,
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => return None,
        }
    }
}

/// Spawn a Kiro ACP shim with a bash command that acts as a mock kiro-cli acp.
fn spawn_kiro_mock(mock_cmd: &str) -> Channel {
    let (parent_sock, child_sock) = protocol::socketpair().unwrap();
    let channel = Channel::new(child_sock);

    let args = ShimArgs {
        id: "kiro-test".into(),
        agent_type: crate::shim::classifier::AgentType::Kiro,
        cmd: mock_cmd.to_string(),
        cwd: PathBuf::from("/tmp"),
        rows: 24,
        cols: 80,
        pty_log_path: None,
        graceful_shutdown_timeout_secs: 5,
        auto_commit_on_restart: true,
    };

    std::thread::spawn(move || {
        crate::shim::runtime_kiro::run_kiro_acp(args, channel).ok();
    });

    let mut ch = Channel::new(parent_sock);
    ch.set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    ch
}

/// Write a mock ACP script to a temp file and return the command to execute it.
///
/// Avoids all bash quoting issues by writing the script to disk.
/// The handshake extracts JSON-RPC request IDs dynamically so tests work
/// regardless of the global AtomicU64 counter value.
fn write_mock_script(prompt_handler: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static MOCK_COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "{}-{}",
        std::process::id(),
        MOCK_COUNTER.fetch_add(1, Ordering::Relaxed),
    );
    let script_path = std::env::temp_dir().join(format!("batty-kiro-mock-{unique}.sh"));

    let script = format!(
        r#"#!/bin/bash
# Helper to extract JSON-RPC id from a request line
extract_id() {{
    echo "$1" | grep -o '"id":[0-9]*' | head -1 | grep -o '[0-9]*'
}}

# --- ACP initialization handshake ---

# Read initialize request
read line
init_id=$(extract_id "$line")
echo '{{"jsonrpc":"2.0","id":'$init_id',"result":{{"protocolVersion":1,"agentCapabilities":{{}},"agentInfo":{{"name":"mock-kiro","version":"1.0.0"}}}}}}'

# Read session/new request
read line
sess_id=$(extract_id "$line")
echo '{{"jsonrpc":"2.0","id":'$sess_id',"result":{{"sessionId":"sess-mock-123"}}}}'

# --- prompt handling ---
{prompt_handler}
"#
    );

    std::fs::write(&script_path, &script).expect("failed to write mock script");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).ok();
    }

    format!("exec bash {}", script_path.display())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Kiro ACP mode completes the handshake and emits Ready.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_emits_ready_after_handshake() {
    let mock = write_mock_script("sleep 30");
    let mut ch = spawn_kiro_mock(&mock);

    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    });
    assert!(evt.is_some(), "expected Ready event after ACP handshake");

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Kiro ACP mode responds to Ping with Pong.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_ping_pong() {
    let mock = write_mock_script("sleep 30");
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::Ping).unwrap();
    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::Pong)
    });
    assert!(evt.is_some(), "expected Pong");

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Kiro ACP mode reports state as Idle after Ready.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_get_state_idle() {
    let mock = write_mock_script("sleep 30");
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::GetState).unwrap();
    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::State { .. })
    });
    match evt {
        Some(Event::State { state, .. }) => assert_eq!(state, ShimState::Idle),
        _ => panic!("expected State event"),
    }

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Sending a prompt produces a Completion with streamed text.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_prompt_produces_completion() {
    let mock = write_mock_script(
        r#"
read line
prompt_id=$(extract_id "$line")
echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-mock-123","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello from mock Kiro"}}}}'
sleep 0.1
echo '{"jsonrpc":"2.0","id":'$prompt_id',"result":{"stopReason":"end_turn"}}'
sleep 30
"#,
    );
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "Hello".into(),
        message_id: Some("msg-1".into()),
    })
    .unwrap();

    // Should get Working state
    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    });
    assert!(evt.is_some(), "expected Working state change");

    // Wait for Completion
    let mut got_completion = false;
    let mut got_idle = false;
    let mut response_text = String::new();
    let mut completion_msg_id = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !(got_completion && got_idle) && std::time::Instant::now() < deadline {
        match ch.recv::<Event>() {
            Ok(Some(Event::Completion {
                response,
                message_id,
                ..
            })) => {
                response_text = response;
                completion_msg_id = message_id;
                got_completion = true;
            }
            Ok(Some(Event::StateChanged {
                to: ShimState::Idle,
                ..
            })) => {
                got_idle = true;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => break,
        }
    }
    assert!(got_completion, "expected Completion event");
    assert!(got_idle, "expected Idle state after completion");
    assert!(
        response_text.contains("Hello from mock Kiro"),
        "response should contain mock text, got: {response_text}"
    );
    assert_eq!(completion_msg_id.as_deref(), Some("msg-1"));

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Permission requests are auto-approved.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_auto_approves_permissions() {
    let mock = write_mock_script(
        r#"
read line
prompt_id=$(extract_id "$line")
echo '{"jsonrpc":"2.0","id":100,"method":"session/request_permission","params":{"sessionId":"sess-mock-123","toolCall":{"toolCallId":"c1","title":"Running: ls","kind":"execute"},"options":[{"optionId":"allow_once","name":"Yes"},{"optionId":"deny","name":"No"}]}}'
read approval
if echo "$approval" | grep -q "allow_once"; then
  echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-mock-123","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"permission approved"}}}}'
  sleep 0.1
  echo '{"jsonrpc":"2.0","id":'$prompt_id',"result":{"stopReason":"end_turn"}}'
else
  echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-mock-123","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"permission denied"}}}}'
  sleep 0.1
  echo '{"jsonrpc":"2.0","id":'$prompt_id',"result":{"stopReason":"end_turn"}}'
fi
sleep 30
"#,
    );
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "run a command".into(),
        message_id: None,
    })
    .unwrap();

    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Completion { .. })
    });
    match evt {
        Some(Event::Completion { response, .. }) => {
            assert!(
                response.contains("permission approved"),
                "expected auto-approval, got: {response}"
            );
        }
        _ => panic!("expected Completion"),
    }

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// When the mock subprocess exits, Died is emitted.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_subprocess_exit_emits_died() {
    let mock = write_mock_script("sleep 0.5; exit 0");
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    let evt = wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Died { .. })
    });
    assert!(evt.is_some(), "expected Died event after subprocess exit");
}

/// Messages sent while Working are queued and delivered on Idle.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_message_queued_while_working() {
    let mock = write_mock_script(
        r#"
while read line; do
  req_id=$(extract_id "$line")
  echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-mock-123","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"response"}}}}'
  sleep 0.1
  echo '{"jsonrpc":"2.0","id":'$req_id',"result":{"stopReason":"end_turn"}}'
done
"#,
    );
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    // Send first message
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m1".into()),
    })
    .unwrap();

    // Wait for Working state
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    })
    .expect("Working after first message");

    // Send second message while working
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "second".into(),
        message_id: Some("m2".into()),
    })
    .unwrap();

    // Should get Warning
    let evt = wait_for_event(&mut ch, Duration::from_secs(3), |e| {
        matches!(e, Event::Warning { .. })
    });
    assert!(evt.is_some(), "expected Warning about queued message");

    // Wait for first Completion
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Completion { .. })
    })
    .expect("first Completion");

    // Queued message should produce second Completion
    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Completion { .. })
    });
    assert!(
        evt.is_some(),
        "expected second Completion from queued message"
    );

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// A prompt that fails with a JSON-RPC error still delivers the next queued
/// message, as a normal completion does.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_error_completion_delivers_queued_message() {
    let mock = write_mock_script(
        r#"
read line
req_id=$(extract_id "$line")
sleep 0.5
echo '{"jsonrpc":"2.0","id":'$req_id',"error":{"code":-32000,"message":"boom"}}'
read line
req_id=$(extract_id "$line")
echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-mock-123","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"second ok"}}}}'
echo '{"jsonrpc":"2.0","id":'$req_id',"result":{"stopReason":"end_turn"}}'
sleep 30
"#,
    );
    let mut ch = spawn_kiro_mock(&mock);
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m1".into()),
    })
    .unwrap();
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    })
    .expect("Working after first message");
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "second".into(),
        message_id: Some("m2".into()),
    })
    .unwrap();

    match wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Completion { .. })
    }) {
        Some(Event::Completion {
            message_id,
            response,
            ..
        }) => {
            assert_eq!(message_id.as_deref(), Some("m1"));
            assert_eq!(response, "[error] boom");
        }
        other => panic!("expected error Completion, got {other:?}"),
    }
    match wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Completion { .. })
    }) {
        Some(Event::Completion {
            message_id,
            response,
            ..
        }) => {
            assert_eq!(message_id.as_deref(), Some("m2"));
            assert_eq!(response, "second ok");
        }
        other => panic!("queued message was not delivered, got {other:?}"),
    }

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// A queued message whose stdin write fails after a turn ends is reported
/// as failed and the shim returns to Idle instead of waiting in Working.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_queued_message_write_failure_returns_to_idle() {
    let mock = write_mock_script(
        r#"
read line
req_id=$(extract_id "$line")
exec 0<&-
sleep 0.5
echo '{"jsonrpc":"2.0","id":'$req_id',"result":{"stopReason":"end_turn"}}'
sleep 30
"#,
    );
    let mut ch = spawn_kiro_mock(&mock);
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m1".into()),
    })
    .unwrap();
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    })
    .expect("Working after first message");
    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "second".into(),
        message_id: Some("m2".into()),
    })
    .unwrap();

    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(e, Event::Error { reason, .. }
            if reason == "stdin write failed for queued message (id: m2): closed")
    })
    .expect("queued write failure reported");
    ch.send(&Command::GetState).unwrap();
    let events = drain_events(&mut ch);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::State {
                state: ShimState::Idle,
                ..
            }
        )),
        "expected shim back in Idle: {events:?}"
    );

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Context exhaustion detected from high usage metadata notification.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_context_exhaustion_from_metadata() {
    let mock = write_mock_script(
        r#"
read line
echo '{"jsonrpc":"2.0","method":"_kiro.dev/metadata","params":{"sessionId":"sess-mock-123","credits":1.0,"contextUsagePercentage":99.5}}'
sleep 30
"#,
    );
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "test".into(),
        message_id: None,
    })
    .unwrap();

    let evt = wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::ContextExhausted { .. })
    });
    assert!(evt.is_some(), "expected ContextExhausted from high usage");

    ch.send(&Command::Kill).ok();
}

/// CaptureScreen returns accumulated response text.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_capture_screen() {
    let mock = write_mock_script("sleep 30");
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::CaptureScreen {
        last_n_lines: Some(10),
    })
    .unwrap();

    let evt = wait_for_event(&mut ch, Duration::from_secs(2), |e| {
        matches!(e, Event::ScreenCapture { .. })
    });
    match evt {
        Some(Event::ScreenCapture { content, .. }) => {
            assert!(content.is_empty(), "expected empty screen before messages");
        }
        _ => panic!("expected ScreenCapture"),
    }

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Shutdown terminates cleanly.
///
/// After shutdown the channel may close (shim exited) or we may receive
/// Died/Dead events — either outcome means the shutdown was clean.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_shutdown_terminates_cleanly() {
    // Use `cat` instead of `sleep` — cat exits when stdin closes
    let mock = write_mock_script("cat > /dev/null");
    let mut ch = spawn_kiro_mock(&mock);

    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::Shutdown {
        timeout_secs: 3,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .unwrap();

    // The shim may emit Died/Dead or just close the channel — both are fine.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut shutdown_clean = false;
    while std::time::Instant::now() < deadline {
        match ch.recv::<Event>() {
            Ok(Some(Event::Died { .. }))
            | Ok(Some(Event::StateChanged {
                to: ShimState::Dead,
                ..
            })) => {
                shutdown_clean = true;
                break;
            }
            Ok(None) => {
                // Channel closed — shim exited cleanly
                shutdown_clean = true;
                break;
            }
            Err(e) if is_timeout_error(&e) => continue,
            Err(_) => {
                // Channel error — shim exited
                shutdown_clean = true;
                break;
            }
            Ok(Some(_)) => continue,
        }
    }
    assert!(shutdown_clean, "expected clean shutdown");
}

/// Read events until the channel goes quiet (read timeout) or closes.
fn drain_events(ch: &mut Channel) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        match ch.recv::<Event>() {
            Ok(Some(evt)) => events.push(evt),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    events
}

/// Characterization: an Idle SendMessage reports MessageDelivered before the
/// Idle -> Working StateChanged.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_idle_send_message_emits_delivered_then_state_changed() {
    let mock = write_mock_script("while read line; do :; done");
    let mut ch = spawn_kiro_mock(&mock);
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m1".into()),
    })
    .unwrap();

    let events = drain_events(&mut ch);
    assert!(
        matches!(&events[..], [
            Event::MessageDelivered { id },
            Event::StateChanged {
                from: ShimState::Idle,
                to: ShimState::Working,
                ..
            },
            ..
        ] if id == "m1"),
        "unexpected events: {events:?}"
    );

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// Characterization: filling the queue past MAX_QUEUE_DEPTH (16) while Working
/// drops the oldest message with an Error, then still emits the queued Warning.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_queue_overflow_drops_oldest_with_error_then_warning() {
    // Mock never answers the prompt, so the agent stays Working.
    let mock = write_mock_script("while read line; do :; done");
    let mut ch = spawn_kiro_mock(&mock);
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m0".into()),
    })
    .unwrap();
    wait_for_event(&mut ch, Duration::from_secs(5), |e| {
        matches!(
            e,
            Event::StateChanged {
                to: ShimState::Working,
                ..
            }
        )
    })
    .expect("Working");

    for i in 1..=17 {
        ch.send(&Command::SendMessage {
            from: "user".into(),
            body: format!("queued {i}"),
            message_id: Some(format!("q{i}")),
        })
        .unwrap();
    }

    let events = drain_events(&mut ch);
    assert_eq!(events.len(), 18, "unexpected events: {events:?}");
    for (i, evt) in events[..16].iter().enumerate() {
        match evt {
            Event::Warning { message, idle_secs } => {
                assert_eq!(
                    message,
                    &format!("message queued while agent working (depth: {})", i + 1)
                );
                assert!(idle_secs.is_none());
            }
            other => panic!("expected Warning at {i}, got {other:?}"),
        }
    }
    match &events[16] {
        Event::Error { command, reason } => {
            assert_eq!(command, "SendMessage");
            assert_eq!(
                reason,
                "message queue full (16), dropped oldest message (id: q1)"
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }
    match &events[17] {
        Event::Warning { message, .. } => {
            assert_eq!(message, "message queued while agent working (depth: 16)");
        }
        other => panic!("expected Warning, got {other:?}"),
    }

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}

/// A failed stdin write must not strand the shim in Working: the message is
/// reported as failed, never as delivered, and the shim stays Idle.
#[test]
#[cfg_attr(not(feature = "shim-integration"), ignore)]
fn kiro_acp_stdin_write_failure_returns_to_idle() {
    // After the handshake the mock closes stdin so prompt writes fail.
    let mock = write_mock_script("exec 0<&-\nsleep 30");
    let mut ch = spawn_kiro_mock(&mock);
    wait_for_event(&mut ch, Duration::from_secs(10), |e| {
        matches!(e, Event::Ready)
    })
    .expect("Ready");
    std::thread::sleep(Duration::from_millis(500));

    ch.send(&Command::SendMessage {
        from: "user".into(),
        body: "first".into(),
        message_id: Some("m1".into()),
    })
    .unwrap();
    ch.send(&Command::GetState).unwrap();

    let events = drain_events(&mut ch);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::DeliveryFailed { id, .. } if id == "m1")),
        "expected DeliveryFailed: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::MessageDelivered { .. })),
        "unwritten message reported as delivered: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::State {
                state: ShimState::Idle,
                ..
            }
        )),
        "expected shim back in Idle: {events:?}"
    );

    ch.send(&Command::Shutdown {
        timeout_secs: 2,
        reason: crate::shim::protocol::ShutdownReason::Requested,
    })
    .ok();
}
