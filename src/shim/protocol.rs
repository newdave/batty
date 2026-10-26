//! Wire protocol: Commands (orchestrator→shim) and Events (shim→orchestrator).
//!
//! Transport: length-prefixed JSON over a Unix SOCK_STREAM socketpair.
//! 4-byte big-endian length prefix + JSON payload.

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

// ---------------------------------------------------------------------------
// Commands (sent TO the shim)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd")]
pub enum Command {
    SendMessage {
        from: String,
        body: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    CaptureScreen {
        last_n_lines: Option<usize>,
    },
    GetState,
    Resize {
        rows: u16,
        cols: u16,
    },
    Shutdown {
        timeout_secs: u32,
        #[serde(default)]
        reason: ShutdownReason,
    },
    Kill,
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    #[default]
    Requested,
    RestartHandoff,
    ContextExhausted,
    TopologyChange,
    DaemonStop,
}

impl ShutdownReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::RestartHandoff => "restart_handoff",
            Self::ContextExhausted => "context_exhausted",
            Self::TopologyChange => "topology_change",
            Self::DaemonStop => "daemon_stop",
        }
    }
}

// ---------------------------------------------------------------------------
// Events (sent FROM the shim)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    Ready,
    StateChanged {
        from: ShimState,
        to: ShimState,
        summary: String,
    },
    MessageDelivered {
        id: String,
    },
    Completion {
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        response: String,
        last_lines: String,
    },
    Died {
        exit_code: Option<i32>,
        last_lines: String,
    },
    ContextExhausted {
        message: String,
        last_lines: String,
    },
    ContextWarning {
        model: Option<String>,
        output_bytes: u64,
        uptime_secs: u64,
        input_tokens: u64,
        cached_input_tokens: u64,
        cache_creation_input_tokens: u64,
        cache_read_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        used_tokens: u64,
        context_limit_tokens: u64,
        usage_pct: u8,
    },
    ContextApproaching {
        message: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    QuotaBlocked {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_at_epoch_secs: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_at_label: Option<String>,
    },
    /// Backend authentication is non-recoverable — a human must re-login
    /// before the agent can continue. Emitted when the shim detects a
    /// refresh-token or credential error that respawning cannot fix.
    AuthRequired {
        message: String,
    },
    /// A terminal backend setting error. Repair configuration before retrying;
    /// re-authentication and repeated respawns do not resolve this condition.
    ConfigurationRequired {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    ScreenCapture {
        content: String,
        cursor_row: u16,
        cursor_col: u16,
    },
    State {
        state: ShimState,
        since_secs: u64,
    },
    /// Actual provider thread identity, never a locally invented placeholder.
    SessionStarted {
        session_id: String,
    },
    SessionStats {
        output_bytes: u64,
        uptime_secs: u64,
        #[serde(default)]
        input_tokens: u64,
        #[serde(default)]
        output_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_usage_pct: Option<u8>,
    },
    Pong,
    Warning {
        message: String,
        idle_secs: Option<u64>,
    },
    DeliveryFailed {
        id: String,
        reason: String,
    },
    Error {
        command: String,
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Shim state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShimState {
    Starting,
    Idle,
    Working,
    Dead,
    ContextExhausted,
}

impl std::fmt::Display for ShimState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => write!(f, "starting"),
            Self::Idle => write!(f, "idle"),
            Self::Working => write!(f, "working"),
            Self::Dead => write!(f, "dead"),
            Self::ContextExhausted => write!(f, "context_exhausted"),
        }
    }
}

// ---------------------------------------------------------------------------
// Framed channel over a Unix socket
// ---------------------------------------------------------------------------

/// Blocking, length-prefixed JSON channel over a Unix stream socket.
///
/// Uses 4-byte big-endian length + JSON payload for robustness.
pub struct Channel {
    stream: UnixStream,
    read_buf: Vec<u8>,
    /// Unsent tail of a frame whose write timed out mid-frame. It must be
    /// flushed before any new frame, otherwise the length-prefixed stream
    /// would be corrupted by interleaving.
    pending_out: Vec<u8>,
}

const MAX_MSG: usize = 1_048_576; // 1 MB

impl Channel {
    pub fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            read_buf: vec![0u8; 4096],
            pending_out: Vec::new(),
        }
    }

    /// Write `buf`, returning how many bytes were written. On a write
    /// timeout / EAGAIN the bytes already written are reported as success so
    /// the caller can keep the remainder instead of re-sending the frame.
    fn write_partial(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut written = 0;
        while written < buf.len() {
            match self.stream.write(&buf[written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e)
                    if written > 0
                        && matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(written)
    }

    /// Send a serializable message.
    ///
    /// A write timeout before any byte of the frame is written returns an
    /// error with nothing sent (safe to retry). A timeout after a partial
    /// write returns `Ok`: the frame is committed and its remainder is
    /// flushed before the next frame, so the stream is never corrupted and
    /// the message is never duplicated.
    pub fn send<T: Serialize>(&mut self, msg: &T) -> anyhow::Result<()> {
        let json = serde_json::to_vec(msg)?;
        if json.len() > MAX_MSG {
            anyhow::bail!("message too large: {} bytes", json.len());
        }
        if !self.flush_pending()? {
            return Err(io::Error::from(io::ErrorKind::WouldBlock).into());
        }
        let mut frame = Vec::with_capacity(4 + json.len());
        frame.extend_from_slice(&(json.len() as u32).to_be_bytes());
        frame.extend_from_slice(&json);
        let n = self.write_partial(&frame)?;
        if n < frame.len() {
            self.pending_out = frame[n..].to_vec();
        }
        Ok(())
    }

    /// Write any held tail of a partially sent frame. Returns `Ok(true)` once
    /// nothing is pending. Call it periodically so a committed frame does not
    /// sit half-delivered until the next send.
    pub fn flush_pending(&mut self) -> io::Result<bool> {
        if self.pending_out.is_empty() {
            return Ok(true);
        }
        let pending = std::mem::take(&mut self.pending_out);
        match self.write_partial(&pending) {
            Ok(n) => {
                self.pending_out = pending[n..].to_vec();
                Ok(self.pending_out.is_empty())
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                self.pending_out = pending;
                Ok(false)
            }
            Err(e) => {
                self.pending_out = pending;
                Err(e)
            }
        }
    }

    /// Receive a deserializable message. Blocks until a message arrives.
    /// Returns Ok(None) on clean EOF (peer closed).
    pub fn recv<T: for<'de> Deserialize<'de>>(&mut self) -> anyhow::Result<Option<T>> {
        let mut len_buf = [0u8; 4];
        match self.stream.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_MSG {
            anyhow::bail!("incoming message too large: {} bytes", len);
        }
        if self.read_buf.len() < len {
            self.read_buf.resize(len, 0);
        }
        self.stream.read_exact(&mut self.read_buf[..len])?;
        let msg = serde_json::from_slice(&self.read_buf[..len])?;
        Ok(Some(msg))
    }

    /// Set a read timeout on the underlying socket.
    /// After this, `recv()` will return an error if no data arrives
    /// within the given duration (instead of blocking forever).
    pub fn set_read_timeout(&mut self, timeout: Option<std::time::Duration>) -> anyhow::Result<()> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    /// Set a write timeout on the underlying socket.
    ///
    /// Without this, `send()` calls `write_all()` which blocks indefinitely
    /// when the peer stops draining its receive buffer — e.g. a wedged shim
    /// that stopped reading. A blocking `write_all` inside the daemon's
    /// `poll_shim_handles` / ping_pong tick wedges the ENTIRE daemon event
    /// loop on a single stuck handle, which matches the documented "daemon
    /// freezes after 10-15 min productive window" failure mode. Setting a
    /// bounded write timeout turns that hard-hang into a regular `send()`
    /// error that the caller can classify as a stale / dead handle and
    /// escalate via the usual respawn / crash paths.
    pub fn set_write_timeout(
        &mut self,
        timeout: Option<std::time::Duration>,
    ) -> anyhow::Result<()> {
        self.stream.set_write_timeout(timeout)?;
        Ok(())
    }

    /// Clone the underlying fd for use in a second thread.
    pub fn try_clone(&self) -> anyhow::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
            read_buf: vec![0u8; 4096],
            pending_out: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// Create a connected socketpair
// ---------------------------------------------------------------------------

/// Create a connected pair of Unix stream sockets.
/// Returns (parent_socket, child_socket).
pub fn socketpair() -> anyhow::Result<(UnixStream, UnixStream)> {
    let (a, b) = UnixStream::pair()?;
    Ok((a, b))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_command_send_message() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::SendMessage {
            from: "user".into(),
            body: "say hello".into(),
            message_id: Some("msg-1".into()),
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();

        match received {
            Command::SendMessage {
                from,
                body,
                message_id,
            } => {
                assert_eq!(from, "user");
                assert_eq!(body, "say hello");
                assert_eq!(message_id.as_deref(), Some("msg-1"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_command_capture_screen() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::CaptureScreen {
            last_n_lines: Some(10),
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        match received {
            Command::CaptureScreen { last_n_lines } => assert_eq!(last_n_lines, Some(10)),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_command_get_state() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Command::GetState).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        assert!(matches!(received, Command::GetState));
    }

    #[test]
    fn roundtrip_command_resize() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::Resize {
            rows: 50,
            cols: 220,
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        match received {
            Command::Resize { rows, cols } => {
                assert_eq!(rows, 50);
                assert_eq!(cols, 220);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_command_shutdown() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let cmd = Command::Shutdown {
            timeout_secs: 30,
            reason: ShutdownReason::Requested,
        };
        sender.send(&cmd).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        match received {
            Command::Shutdown {
                timeout_secs,
                reason,
            } => {
                assert_eq!(timeout_secs, 30);
                assert_eq!(reason, ShutdownReason::Requested);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn shutdown_reason_labels_restart_handoff_explicitly() {
        assert_eq!(ShutdownReason::RestartHandoff.label(), "restart_handoff");
        assert_ne!(
            ShutdownReason::RestartHandoff.label(),
            "orchestrator disconnected"
        );
    }

    #[test]
    fn roundtrip_command_kill() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Command::Kill).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        assert!(matches!(received, Command::Kill));
    }

    #[test]
    fn roundtrip_command_ping() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Command::Ping).unwrap();
        let received: Command = receiver.recv::<Command>().unwrap().unwrap();
        assert!(matches!(received, Command::Ping));
    }

    #[test]
    fn roundtrip_event_completion() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Completion {
            message_id: None,
            response: "Hello!".into(),
            last_lines: "Hello!\n❯".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();

        match received {
            Event::Completion { response, .. } => assert_eq!(response, "Hello!"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_message_delivered() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::MessageDelivered { id: "msg-1".into() };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();

        match received {
            Event::MessageDelivered { id } => assert_eq!(id, "msg-1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_state_changed() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::StateChanged {
            from: ShimState::Idle,
            to: ShimState::Working,
            summary: "working now".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::StateChanged { from, to, summary } => {
                assert_eq!(from, ShimState::Idle);
                assert_eq!(to, ShimState::Working);
                assert_eq!(summary, "working now");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_ready() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Event::Ready).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        assert!(matches!(received, Event::Ready));
    }

    #[test]
    fn roundtrip_event_pong() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        sender.send(&Event::Pong).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        assert!(matches!(received, Event::Pong));
    }

    #[test]
    fn roundtrip_event_delivery_failed() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::DeliveryFailed {
            id: "msg-1".into(),
            reason: "stdin write failed".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();

        match received {
            Event::DeliveryFailed { id, reason } => {
                assert_eq!(id, "msg-1");
                assert_eq!(reason, "stdin write failed");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_died() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Died {
            exit_code: Some(1),
            last_lines: "error occurred".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::Died {
                exit_code,
                last_lines,
            } => {
                assert_eq!(exit_code, Some(1));
                assert_eq!(last_lines, "error occurred");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_context_exhausted() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ContextExhausted {
            message: "context full".into(),
            last_lines: "last output".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ContextExhausted {
                message,
                last_lines,
            } => {
                assert_eq!(message, "context full");
                assert_eq!(last_lines, "last output");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_screen_capture() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ScreenCapture {
            content: "screen data".into(),
            cursor_row: 5,
            cursor_col: 10,
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ScreenCapture {
                content,
                cursor_row,
                cursor_col,
            } => {
                assert_eq!(content, "screen data");
                assert_eq!(cursor_row, 5);
                assert_eq!(cursor_col, 10);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_context_warning() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ContextWarning {
            model: Some("claude-sonnet-4-5".into()),
            output_bytes: 12_345,
            uptime_secs: 61,
            input_tokens: 80_000,
            cached_input_tokens: 5_000,
            cache_creation_input_tokens: 4_000,
            cache_read_input_tokens: 3_000,
            output_tokens: 6_000,
            reasoning_output_tokens: 2_000,
            used_tokens: 100_000,
            context_limit_tokens: 200_000,
            usage_pct: 50,
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ContextWarning {
                model,
                output_bytes,
                uptime_secs,
                input_tokens,
                cached_input_tokens,
                cache_creation_input_tokens,
                cache_read_input_tokens,
                output_tokens,
                reasoning_output_tokens,
                used_tokens,
                context_limit_tokens,
                usage_pct,
            } => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-5"));
                assert_eq!(output_bytes, 12_345);
                assert_eq!(uptime_secs, 61);
                assert_eq!(input_tokens, 80_000);
                assert_eq!(cached_input_tokens, 5_000);
                assert_eq!(cache_creation_input_tokens, 4_000);
                assert_eq!(cache_read_input_tokens, 3_000);
                assert_eq!(output_tokens, 6_000);
                assert_eq!(reasoning_output_tokens, 2_000);
                assert_eq!(used_tokens, 100_000);
                assert_eq!(context_limit_tokens, 200_000);
                assert_eq!(usage_pct, 50);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_session_stats() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::SessionStats {
            output_bytes: 123_456,
            uptime_secs: 61,
            input_tokens: 5000,
            output_tokens: 1200,
            context_usage_pct: Some(84),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::SessionStats {
                output_bytes,
                uptime_secs,
                input_tokens,
                output_tokens,
                context_usage_pct,
            } => {
                assert_eq!(output_bytes, 123_456);
                assert_eq!(uptime_secs, 61);
                assert_eq!(input_tokens, 5000);
                assert_eq!(output_tokens, 1200);
                assert_eq!(context_usage_pct, Some(84));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_context_approaching() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::ContextApproaching {
            message: "context pressure detected".into(),
            input_tokens: 80000,
            output_tokens: 20000,
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::ContextApproaching {
                message,
                input_tokens,
                output_tokens,
            } => {
                assert_eq!(message, "context pressure detected");
                assert_eq!(input_tokens, 80000);
                assert_eq!(output_tokens, 20000);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_quota_blocked() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::QuotaBlocked {
            message: "usage limit reached".into(),
            retry_at_epoch_secs: Some(1_776_214_440),
            retry_at_label: Some("2026-04-16 12:54".into()),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::QuotaBlocked {
                message,
                retry_at_epoch_secs,
                retry_at_label,
            } => {
                assert_eq!(message, "usage limit reached");
                assert_eq!(retry_at_epoch_secs, Some(1_776_214_440));
                assert_eq!(retry_at_label.as_deref(), Some("2026-04-16 12:54"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_error() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Error {
            command: "SendMessage".into(),
            reason: "agent busy".into(),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::Error { command, reason } => {
                assert_eq!(command, "SendMessage");
                assert_eq!(reason, "agent busy");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn roundtrip_event_warning() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        let mut receiver = Channel::new(b);

        let evt = Event::Warning {
            message: "no screen change".into(),
            idle_secs: Some(300),
        };
        sender.send(&evt).unwrap();
        let received: Event = receiver.recv::<Event>().unwrap().unwrap();
        match received {
            Event::Warning { message, idle_secs } => {
                assert_eq!(message, "no screen change");
                assert_eq!(idle_secs, Some(300));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn eof_returns_none() {
        let (a, b) = socketpair().unwrap();
        drop(a); // close sender
        let mut receiver = Channel::new(b);
        let result: Option<Command> = receiver.recv().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn all_states_serialize() {
        for state in [
            ShimState::Starting,
            ShimState::Idle,
            ShimState::Working,
            ShimState::Dead,
            ShimState::ContextExhausted,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let back: ShimState = serde_json::from_str(&json).unwrap();
            assert_eq!(state, back);
        }
    }

    #[test]
    fn shim_state_display() {
        assert_eq!(ShimState::Starting.to_string(), "starting");
        assert_eq!(ShimState::Idle.to_string(), "idle");
        assert_eq!(ShimState::Working.to_string(), "working");
        assert_eq!(ShimState::Dead.to_string(), "dead");
        assert_eq!(ShimState::ContextExhausted.to_string(), "context_exhausted");
    }

    #[test]
    fn socketpair_creates_connected_pair() {
        let (a, b) = socketpair().unwrap();
        // Basic connectivity: write on a, read on b
        let mut ch_a = Channel::new(a);
        let mut ch_b = Channel::new(b);
        ch_a.send(&Command::Ping).unwrap();
        let msg: Command = ch_b.recv().unwrap().unwrap();
        assert!(matches!(msg, Command::Ping));
    }

    #[test]
    fn partial_write_timeout_does_not_corrupt_frame_stream() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        sender
            .set_write_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut receiver = Channel::new(b);

        let make = |tag: &str| Command::SendMessage {
            from: "daemon".into(),
            body: format!("{tag}{}", "x".repeat(512 * 1024)),
            message_id: None,
        };
        // Nobody is reading, so this frame is written only partially; the
        // send is committed (Ok) and its tail is held for the next send.
        sender.send(&make("A")).unwrap();
        assert!(!sender.pending_out.is_empty());

        let reader = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                match receiver.recv::<Command>().unwrap().unwrap() {
                    Command::SendMessage { body, .. } => bodies.push(body),
                    other => panic!("unexpected {other:?}"),
                }
            }
            bodies
        });
        let start = std::time::Instant::now();
        while sender.send(&make("B")).is_err() {
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let bodies = reader.join().unwrap();
        assert!(bodies[0].starts_with('A') && bodies[0].len() == 512 * 1024 + 1);
        assert!(bodies[1].starts_with('B') && bodies[1].len() == 512 * 1024 + 1);
    }

    #[test]
    fn flush_pending_completes_partial_frame_without_another_send() {
        let (a, b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        sender
            .set_write_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut receiver = Channel::new(b);
        assert!(sender.flush_pending().unwrap(), "nothing pending initially");

        sender
            .send(&Command::SendMessage {
                from: "daemon".into(),
                body: format!("A{}", "x".repeat(512 * 1024)),
                message_id: None,
            })
            .unwrap();
        assert!(!sender.pending_out.is_empty());

        let reader = std::thread::spawn(move || receiver.recv::<Command>().unwrap().unwrap());
        let start = std::time::Instant::now();
        while !sender.flush_pending().unwrap() {
            assert!(start.elapsed() < std::time::Duration::from_secs(10));
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        match reader.join().unwrap() {
            Command::SendMessage { body, .. } => {
                assert!(body.starts_with('A') && body.len() == 512 * 1024 + 1)
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn send_with_nothing_written_on_timeout_is_retryable_error() {
        let (a, _b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        sender
            .set_write_timeout(Some(std::time::Duration::from_millis(20)))
            .unwrap();
        let cmd = Command::SendMessage {
            from: "d".into(),
            body: "x".repeat(512 * 1024),
            message_id: None,
        };
        sender.send(&cmd).unwrap(); // partial, committed
        let err = sender.send(&cmd).unwrap_err();
        assert!(!sender.pending_out.is_empty());
        assert!(err.downcast_ref::<io::Error>().is_some_and(|e| matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )));
    }

    #[test]
    fn send_times_out_when_peer_stops_reading() {
        // Regression test for the documented "daemon freezes after 10-15 min
        // productive window" pattern. Without a write timeout, a wedged shim
        // that stops draining its socket buffer will cause the daemon's next
        // `send()` to block inside `write_all` indefinitely, wedging the
        // entire event loop on a single stuck handle.
        //
        // We simulate the wedge by filling the peer's receive buffer beyond
        // capacity and never calling `recv()` on the other side. With a
        // short write timeout set, `send()` must return an error within a
        // bounded window instead of hanging forever.
        let (a, _b) = socketpair().unwrap();
        let mut sender = Channel::new(a);
        sender
            .set_write_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();

        // Build a large but legal payload and blast it across the pipe
        // until the kernel refuses more bytes. On Linux the default per-pipe
        // buffer is a few hundred KB; on macOS it's ~8 KB. Either way, a
        // bounded retry loop must eventually hit the write timeout instead
        // of blocking forever.
        let big_body = "x".repeat(256 * 1024);
        let cmd = Command::SendMessage {
            from: "daemon".into(),
            body: big_body,
            message_id: None,
        };

        let start = std::time::Instant::now();
        let mut attempts = 0;
        let mut last_err = None;
        while start.elapsed() < std::time::Duration::from_secs(5) {
            attempts += 1;
            match sender.send(&cmd) {
                Ok(()) => continue,
                Err(error) => {
                    last_err = Some(error);
                    break;
                }
            }
        }
        let error = last_err.expect("send should have timed out within 5s");
        let io_error = error
            .downcast_ref::<std::io::Error>()
            .expect("write timeout should surface as an io::Error");
        assert!(
            matches!(
                io_error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ),
            "expected WouldBlock/TimedOut error, got {:?}",
            io_error.kind()
        );
        assert!(
            attempts >= 1,
            "sanity check: send loop should have attempted at least once"
        );
    }
    #[test]
    fn configuration_required_event_roundtrips_separately_from_auth() {
        let event = Event::ConfigurationRequired {
            message: "repair model setting".into(),
            code: Some("unsupported_model".into()),
        };
        let value = serde_json::to_value(event).unwrap();
        assert!(matches!(
            serde_json::from_value::<Event>(value).unwrap(),
            Event::ConfigurationRequired { code: Some(_), .. }
        ));
    }
}
