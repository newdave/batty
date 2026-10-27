//! Legacy command queue (`commands.jsonl`): types, append, and atomic draining.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Delivery attempts after which a failing command is dropped.
pub const MAX_COMMAND_ATTEMPTS: u32 = 20;
/// Age of the first failure after which a failing command is dropped.
pub const MAX_COMMAND_RETRY_SECS: u64 = 24 * 60 * 60;

/// A message in the command queue. `attempts` and `first_failed_at` (unix
/// seconds) track retries; both default for entries written before they
/// existed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum QueuedCommand {
    #[serde(rename = "send")]
    Send {
        from: String,
        to: String,
        message: String,
        #[serde(default)]
        attempts: u32,
        #[serde(default)]
        first_failed_at: Option<u64>,
    },
    #[serde(rename = "assign")]
    Assign {
        from: String,
        engineer: String,
        task: String,
        #[serde(default)]
        attempts: u32,
        #[serde(default)]
        first_failed_at: Option<u64>,
    },
}

impl QueuedCommand {
    pub fn send(
        from: impl Into<String>,
        to: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::Send {
            from: from.into(),
            to: to.into(),
            message: message.into(),
            attempts: 0,
            first_failed_at: None,
        }
    }

    pub fn assign(
        from: impl Into<String>,
        engineer: impl Into<String>,
        task: impl Into<String>,
    ) -> Self {
        Self::Assign {
            from: from.into(),
            engineer: engineer.into(),
            task: task.into(),
            attempts: 0,
            first_failed_at: None,
        }
    }

    /// Role the command is addressed to.
    pub fn recipient(&self) -> &str {
        match self {
            Self::Send { to, .. } => to,
            Self::Assign { engineer, .. } => engineer,
        }
    }

    fn retry_state_mut(&mut self) -> (&mut u32, &mut Option<u64>) {
        match self {
            Self::Send {
                attempts,
                first_failed_at,
                ..
            }
            | Self::Assign {
                attempts,
                first_failed_at,
                ..
            } => (attempts, first_failed_at),
        }
    }

    /// Record a failed delivery at `now` (unix secs); true once the retry
    /// budget (attempts or age) is exhausted.
    fn record_failure(&mut self, now: u64) -> bool {
        let (attempts, first_failed_at) = self.retry_state_mut();
        *attempts += 1;
        let first = *first_failed_at.get_or_insert(now);
        *attempts >= MAX_COMMAND_ATTEMPTS || now.saturating_sub(first) >= MAX_COMMAND_RETRY_SECS
    }
}

/// A command given up on by [`drain_with`].
#[derive(Debug, Clone)]
pub struct DroppedCommand {
    pub command: QueuedCommand,
    pub reason: String,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Write a command to the queue file.
pub fn enqueue_command(queue_path: &Path, cmd: &QueuedCommand) -> Result<()> {
    if let Some(parent) = queue_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(cmd)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(queue_path)
        .with_context(|| format!("failed to open command queue: {}", queue_path.display()))?;
    writeln!(file, "{json}")?;
    Ok(())
}

/// Read and drain all pending commands from the queue file.
#[cfg(test)]
pub fn drain_command_queue(queue_path: &Path) -> Result<Vec<QueuedCommand>> {
    let commands = read_command_queue(queue_path)?;
    if !commands.is_empty() {
        write_command_queue(queue_path, &[])?;
    }
    Ok(commands)
}

/// Read all pending commands from the queue file without clearing it.
pub fn read_command_queue(queue_path: &Path) -> Result<Vec<QueuedCommand>> {
    if !queue_path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(queue_path)
        .with_context(|| format!("failed to open command queue: {}", queue_path.display()))?;
    let reader = BufReader::new(file);

    let mut commands = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<QueuedCommand>(trimmed) {
            Ok(cmd) => commands.push(cmd),
            Err(e) => tracing::warn!(line = trimmed, error = %e, "skipping malformed command"),
        }
    }

    Ok(commands)
}

/// Atomically rewrite the command queue with the remaining commands.
pub fn write_command_queue(queue_path: &Path, commands: &[QueuedCommand]) -> Result<()> {
    if let Some(parent) = queue_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = queue_path.with_extension("jsonl.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| {
                format!("failed to open temp command queue: {}", tmp_path.display())
            })?;
        for cmd in commands {
            let json = serde_json::to_string(cmd)?;
            writeln!(file, "{json}")?;
        }
        file.flush()?;
    }

    std::fs::rename(&tmp_path, queue_path).with_context(|| {
        format!(
            "failed to replace command queue {} with {}",
            queue_path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}

/// Path of the batch currently being drained from `queue_path`.
fn draining_path(queue_path: &Path) -> PathBuf {
    queue_path.with_extension("jsonl.draining")
}

/// Sidecar recording how many entries of the `.draining` batch are done.
fn progress_path(queue_path: &Path) -> PathBuf {
    queue_path.with_extension("jsonl.draining.done")
}

fn read_progress(queue_path: &Path) -> usize {
    std::fs::read_to_string(progress_path(queue_path))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn write_progress(queue_path: &Path, done: usize) -> Result<()> {
    let path = progress_path(queue_path);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, done.to_string())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("failed to update {}", path.display()))?;
    Ok(())
}

/// Drain the queue by atomically claiming it, then processing each command.
///
/// The live queue is first renamed to `<queue>.draining`, so commands
/// appended by `enqueue_command` while a batch is being processed land in a
/// fresh live file and are never overwritten. Commands for which `process`
/// returns an error are appended back to the live queue with their attempt
/// count raised; after [`MAX_COMMAND_ATTEMPTS`] attempts or
/// [`MAX_COMMAND_RETRY_SECS`] since the first failure they are dropped with
/// one warning and returned. A sidecar records how many batch entries are
/// done, so a batch left behind by a crash resumes after the entries already
/// handled instead of redelivering them.
pub fn drain_with<F>(queue_path: &Path, process: F) -> Result<Vec<DroppedCommand>>
where
    F: FnMut(&QueuedCommand) -> Result<()>,
{
    drain_with_at(queue_path, unix_now(), process)
}

fn drain_with_at<F>(queue_path: &Path, now: u64, mut process: F) -> Result<Vec<DroppedCommand>>
where
    F: FnMut(&QueuedCommand) -> Result<()>,
{
    let batch_path = draining_path(queue_path);
    if !batch_path.exists() {
        // Any sidecar without a batch is a leftover of a finished drain.
        let _ = std::fs::remove_file(progress_path(queue_path));
        match std::fs::rename(queue_path, &batch_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("failed to claim command queue {}", queue_path.display())
                });
            }
        }
    }

    let commands = read_command_queue(&batch_path)?;
    let already_done = read_progress(queue_path);
    let mut dropped = Vec::new();
    for (index, mut cmd) in commands.into_iter().enumerate().skip(already_done) {
        if let Err(error) = process(&cmd) {
            if cmd.record_failure(now) {
                let reason = format!(
                    "dropped after {} attempt(s): {error:#}",
                    cmd.retry_state_mut().0
                );
                tracing::warn!(to = cmd.recipient(), reason = %reason, "dropping legacy command");
                dropped.push(DroppedCommand {
                    command: cmd,
                    reason,
                });
            } else {
                tracing::debug!(error = %error, "failed to process legacy command; keeping in queue");
                enqueue_command(queue_path, &cmd)?;
            }
        }
        write_progress(queue_path, index + 1)?;
    }
    std::fs::remove_file(&batch_path)
        .with_context(|| format!("failed to remove drained batch {}", batch_path.display()))?;
    let _ = std::fs::remove_file(progress_path(queue_path));
    Ok(dropped)
}

/// Resolve the command queue path.
pub fn command_queue_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".batty")
        .join("team_config")
        .join("commands.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn send(to: &str, message: &str) -> QueuedCommand {
        QueuedCommand::send("human", to, message)
    }

    #[test]
    fn drain_with_retains_only_failures_and_clears_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&queue, &send("ok", "one")).unwrap();
        enqueue_command(&queue, &send("bad", "two")).unwrap();

        drain_with(&queue, |cmd| match cmd {
            QueuedCommand::Send { to, .. } if to == "bad" => anyhow::bail!("nope"),
            _ => Ok(()),
        })
        .unwrap();

        let remaining = read_command_queue(&queue).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(matches!(&remaining[0], QueuedCommand::Send { to, .. } if to == "bad"));
        assert!(!draining_path(&queue).exists());
    }

    #[test]
    fn drain_with_keeps_commands_enqueued_during_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&queue, &send("a", "first")).unwrap();

        let mut seen = Vec::new();
        drain_with(&queue, |cmd| {
            if let QueuedCommand::Send { message, .. } = cmd {
                seen.push(message.clone());
            }
            // A producer appends while the batch is being delivered.
            enqueue_command(&queue, &send("a", "late")).unwrap();
            Ok(())
        })
        .unwrap();

        assert_eq!(seen, ["first"]);
        let remaining = read_command_queue(&queue).unwrap();
        assert_eq!(remaining.len(), 1);
        assert!(matches!(&remaining[0], QueuedCommand::Send { message, .. } if message == "late"));
    }

    #[test]
    fn drain_with_recovers_batch_left_by_crash_without_losing_new_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&draining_path(&queue), &send("a", "stranded")).unwrap();
        enqueue_command(&queue, &send("a", "fresh")).unwrap();

        let mut seen = Vec::new();
        drain_with(&queue, |cmd| {
            if let QueuedCommand::Send { message, .. } = cmd {
                seen.push(message.clone());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, ["stranded"]);
        assert_eq!(read_command_queue(&queue).unwrap().len(), 1);

        drain_with(&queue, |_| Ok(())).unwrap();
        assert!(read_command_queue(&queue).unwrap().is_empty());
    }

    #[test]
    fn drain_with_missing_queue_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        drain_with(&queue, |_| panic!("nothing to process")).unwrap();
        assert!(!draining_path(&queue).exists());
    }

    #[test]
    fn send_command_roundtrip() {
        let cmd = QueuedCommand::send("human", "architect", "prioritize auth");
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send {
                from, to, message, ..
            } => {
                assert_eq!(from, "human");
                assert_eq!(to, "architect");
                assert_eq!(message, "prioritize auth");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn assign_command_roundtrip() {
        let cmd = QueuedCommand::assign("black-lead", "eng-1-1", "fix bug");
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Assign {
                from,
                engineer,
                task,
                ..
            } => {
                assert_eq!(from, "black-lead");
                assert_eq!(engineer, "eng-1-1");
                assert_eq!(task, "fix bug");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn enqueue_and_drain() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        enqueue_command(&queue, &QueuedCommand::send("human", "arch", "hello")).unwrap();
        enqueue_command(
            &queue,
            &QueuedCommand::assign("black-lead", "eng-1", "work"),
        )
        .unwrap();

        let commands = drain_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 2);

        // After drain, queue should be empty
        let commands = drain_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn drain_nonexistent_queue_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("nonexistent.jsonl");
        let commands = drain_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn read_command_queue_keeps_file_contents_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&queue, &QueuedCommand::send("human", "arch", "hello")).unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
        let persisted = std::fs::read_to_string(&queue).unwrap();
        assert!(persisted.contains("\"message\":\"hello\""));
    }

    #[test]
    fn write_command_queue_rewrites_remaining_commands_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&queue, &QueuedCommand::send("human", "arch", "hello")).unwrap();

        write_command_queue(
            &queue,
            &[QueuedCommand::Assign {
                from: "manager".into(),
                engineer: "eng-1".into(),
                task: "Task #1".into(),
                attempts: 0,
                first_failed_at: None,
            }],
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            QueuedCommand::Assign { engineer, task, .. } => {
                assert_eq!(engineer, "eng-1");
                assert_eq!(task, "Task #1");
            }
            other => panic!("expected assign command after rewrite, got {other:?}"),
        }
    }

    #[test]
    fn drain_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            "not json\n{\"type\":\"assign\",\"from\":\"manager\",\"engineer\":\"e1\",\"task\":\"t1\"}\n",
        )
        .unwrap();
        let commands = drain_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn test_drain_command_queue_skips_unknown_and_incomplete_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            concat!(
                "{\"type\":\"noop\",\"from\":\"manager\"}\n",
                "{\"type\":\"send\",\"from\":\"manager\",\"message\":\"missing recipient\"}\n",
                "{\"type\":\"assign\",\"from\":\"manager\",\"engineer\":\"eng-1\"}\n",
                "{\"type\":\"send\",\"from\":\"manager\",\"to\":\"architect\",\"message\":\"valid\"}\n",
            ),
        )
        .unwrap();

        let commands = drain_command_queue(&queue).unwrap();

        assert_eq!(commands.len(), 1);
        match &commands[0] {
            QueuedCommand::Send {
                from, to, message, ..
            } => {
                assert_eq!(from, "manager");
                assert_eq!(to, "architect");
                assert_eq!(message, "valid");
            }
            other => panic!("expected valid send command, got {other:?}"),
        }
    }

    // --- Additional serialization edge cases ---

    #[test]
    fn send_command_preserves_multiline_message() {
        let cmd = QueuedCommand::Send {
            from: "manager".into(),
            to: "eng-1".into(),
            message: "line one\nline two\nline three".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send { message, .. } => {
                assert_eq!(message, "line one\nline two\nline three");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn send_command_preserves_empty_message() {
        let cmd = QueuedCommand::Send {
            from: "human".into(),
            to: "architect".into(),
            message: "".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send { message, .. } => assert!(message.is_empty()),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn send_command_preserves_unicode() {
        let cmd = QueuedCommand::Send {
            from: "人間".into(),
            to: "アーキ".into(),
            message: "日本語テスト 🚀".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Send {
                from, to, message, ..
            } => {
                assert_eq!(from, "人間");
                assert_eq!(to, "アーキ");
                assert_eq!(message, "日本語テスト 🚀");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn assign_command_preserves_special_chars_in_task() {
        let cmd = QueuedCommand::Assign {
            from: "manager".into(),
            engineer: "eng-1-1".into(),
            task: "Task #42: Fix \"auth\" — it's broken!".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: QueuedCommand = serde_json::from_str(&json).unwrap();
        match parsed {
            QueuedCommand::Assign { task, .. } => {
                assert_eq!(task, "Task #42: Fix \"auth\" — it's broken!");
            }
            _ => panic!("wrong variant"),
        }
    }

    // --- enqueue_command edge cases ---

    #[test]
    fn enqueue_creates_parent_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp
            .path()
            .join("deep")
            .join("nested")
            .join("commands.jsonl");

        enqueue_command(&queue, &QueuedCommand::send("human", "arch", "hello")).unwrap();

        assert!(queue.exists());
        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn enqueue_appends_multiple_commands_preserving_order() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        for i in 0..5 {
            enqueue_command(
                &queue,
                &QueuedCommand::Send {
                    from: "human".into(),
                    to: "arch".into(),
                    message: format!("msg-{i}"),
                    attempts: 0,
                    first_failed_at: None,
                },
            )
            .unwrap();
        }

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 5);
        for (i, cmd) in commands.iter().enumerate() {
            match cmd {
                QueuedCommand::Send { message, .. } => {
                    assert_eq!(message, &format!("msg-{i}"));
                }
                _ => panic!("wrong variant at index {i}"),
            }
        }
    }

    // --- read_command_queue edge cases ---

    #[test]
    fn read_command_queue_skips_empty_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            "\n\n{\"type\":\"send\",\"from\":\"a\",\"to\":\"b\",\"message\":\"hi\"}\n\n\n",
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn read_command_queue_skips_whitespace_only_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            "   \n\t\n{\"type\":\"send\",\"from\":\"a\",\"to\":\"b\",\"message\":\"ok\"}\n  \n",
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    #[test]
    fn read_command_queue_mixed_valid_and_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        std::fs::write(
            &queue,
            concat!(
                "{\"type\":\"send\",\"from\":\"a\",\"to\":\"b\",\"message\":\"first\"}\n",
                "garbage line\n",
                "{\"type\":\"assign\",\"from\":\"m\",\"engineer\":\"e1\",\"task\":\"t1\"}\n",
                "{\"invalid json\n",
                "{\"type\":\"send\",\"from\":\"c\",\"to\":\"d\",\"message\":\"last\"}\n",
            ),
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(
            commands.len(),
            3,
            "should parse 3 valid commands, skip 2 malformed"
        );
    }

    // --- write_command_queue edge cases ---

    #[test]
    fn write_command_queue_empty_slice_creates_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        // First enqueue something
        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "a".into(),
                to: "b".into(),
                message: "hello".into(),
                attempts: 0,
                first_failed_at: None,
            },
        )
        .unwrap();

        // Overwrite with empty
        write_command_queue(&queue, &[]).unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn write_command_queue_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("sub").join("dir").join("q.jsonl");

        write_command_queue(
            &queue,
            &[QueuedCommand::Assign {
                from: "m".into(),
                engineer: "e1".into(),
                task: "t1".into(),
                attempts: 0,
                first_failed_at: None,
            }],
        )
        .unwrap();

        let commands = read_command_queue(&queue).unwrap();
        assert_eq!(commands.len(), 1);
    }

    // --- command_queue_path ---

    #[test]
    fn command_queue_path_returns_expected_path() {
        let root = Path::new("/project");
        let path = command_queue_path(root);
        assert_eq!(
            path,
            PathBuf::from("/project/.batty/team_config/commands.jsonl")
        );
    }

    #[test]
    fn command_queue_path_with_trailing_slash() {
        let root = Path::new("/project/");
        let path = command_queue_path(root);
        assert_eq!(
            path,
            PathBuf::from("/project/.batty/team_config/commands.jsonl")
        );
    }

    // --- drain_command_queue ---

    #[test]
    fn drain_empties_queue_file_but_keeps_file() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        enqueue_command(
            &queue,
            &QueuedCommand::Send {
                from: "a".into(),
                to: "b".into(),
                message: "msg".into(),
                attempts: 0,
                first_failed_at: None,
            },
        )
        .unwrap();

        let drained = drain_command_queue(&queue).unwrap();
        assert_eq!(drained.len(), 1);

        // File should still exist but be effectively empty
        assert!(queue.exists());
        let commands = read_command_queue(&queue).unwrap();
        assert!(commands.is_empty());
    }

    #[test]
    fn drain_twice_second_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");

        enqueue_command(
            &queue,
            &QueuedCommand::Assign {
                from: "m".into(),
                engineer: "e".into(),
                task: "t".into(),
                attempts: 0,
                first_failed_at: None,
            },
        )
        .unwrap();

        let first = drain_command_queue(&queue).unwrap();
        assert_eq!(first.len(), 1);

        let second = drain_command_queue(&queue).unwrap();
        assert!(second.is_empty());
    }

    // --- QueuedCommand Debug derive ---

    #[test]
    fn queued_command_debug_format() {
        let cmd = QueuedCommand::Send {
            from: "human".into(),
            to: "arch".into(),
            message: "test".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let debug = format!("{cmd:?}");
        assert!(debug.contains("Send"));
        assert!(debug.contains("human"));
    }

    #[test]
    fn queued_command_clone() {
        let cmd = QueuedCommand::Assign {
            from: "manager".into(),
            engineer: "eng-1".into(),
            task: "build feature".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let cloned = cmd.clone();
        let json_original = serde_json::to_string(&cmd).unwrap();
        let json_cloned = serde_json::to_string(&cloned).unwrap();
        assert_eq!(json_original, json_cloned);
    }

    // --- JSON tag format verification ---

    #[test]
    fn send_command_json_has_type_send_tag() {
        let cmd = QueuedCommand::Send {
            from: "a".into(),
            to: "b".into(),
            message: "c".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"send\""), "got: {json}");
    }

    #[test]
    fn assign_command_json_has_type_assign_tag() {
        let cmd = QueuedCommand::Assign {
            from: "a".into(),
            engineer: "b".into(),
            task: "c".into(),
            attempts: 0,
            first_failed_at: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"type\":\"assign\""), "got: {json}");
    }

    #[test]
    fn failing_command_is_dropped_after_the_attempt_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&queue, &send("nobody", "stuck")).unwrap();

        let mut dropped = Vec::new();
        for round in 1..=MAX_COMMAND_ATTEMPTS {
            dropped = drain_with_at(&queue, 1_000 + u64::from(round), |_| {
                anyhow::bail!("no channel")
            })
            .unwrap();
            if round < MAX_COMMAND_ATTEMPTS {
                assert!(dropped.is_empty(), "dropped too early at round {round}");
                let queued = read_command_queue(&queue).unwrap();
                assert_eq!(queued.len(), 1);
                assert!(matches!(
                    &queued[0],
                    QueuedCommand::Send { attempts, .. } if *attempts == round
                ));
            }
        }
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].command.recipient(), "nobody");
        assert!(dropped[0].reason.contains("20 attempt"));
        assert!(read_command_queue(&queue).unwrap().is_empty());
    }

    #[test]
    fn failing_command_is_dropped_after_a_day() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        enqueue_command(&queue, &send("nobody", "stuck")).unwrap();

        let first = drain_with_at(&queue, 1_000, |_| anyhow::bail!("no")).unwrap();
        assert!(first.is_empty());
        let later = drain_with_at(&queue, 1_000 + MAX_COMMAND_RETRY_SECS, |_| {
            anyhow::bail!("no")
        })
        .unwrap();
        assert_eq!(later.len(), 1);
    }

    #[test]
    fn legacy_entries_without_retry_fields_still_parse() {
        let cmd: QueuedCommand =
            serde_json::from_str(r#"{"type":"send","from":"a","to":"b","message":"m"}"#).unwrap();
        assert!(matches!(
            cmd,
            QueuedCommand::Send {
                attempts: 0,
                first_failed_at: None,
                ..
            }
        ));
    }

    #[test]
    fn crash_after_delivering_two_of_three_redelivers_only_the_third() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        for body in ["one", "two", "three"] {
            enqueue_command(&queue, &send("a", body)).unwrap();
        }

        // First run "crashes" (panics) while handling the third entry.
        let mut delivered = Vec::new();
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = drain_with(&queue, |cmd| {
                if let QueuedCommand::Send { message, .. } = cmd {
                    if message == "three" {
                        panic!("simulated crash");
                    }
                    delivered.push(message.clone());
                }
                Ok(())
            });
        }));
        assert!(crashed.is_err());
        assert_eq!(delivered, ["one", "two"]);
        assert!(draining_path(&queue).exists());

        let mut redelivered = Vec::new();
        drain_with(&queue, |cmd| {
            if let QueuedCommand::Send { message, .. } = cmd {
                redelivered.push(message.clone());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(redelivered, ["three"]);
        assert!(!draining_path(&queue).exists());
        assert!(!progress_path(&queue).exists());
    }

    #[test]
    fn stale_progress_sidecar_does_not_skip_a_new_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let queue = tmp.path().join("commands.jsonl");
        write_progress(&queue, 5).unwrap();
        enqueue_command(&queue, &send("a", "fresh")).unwrap();
        let mut seen = 0;
        drain_with(&queue, |_| {
            seen += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, 1);
    }
}
