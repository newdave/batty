//! JSONL event types for Codex CLI `exec --json` mode.
//!
//! These types model the events emitted on stdout when Codex runs in
//! `codex exec --json` mode. Each line is a complete JSON object with
//! a `type` tag discriminating the event kind.

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Top-level thread events (one per JSONL line)
// ---------------------------------------------------------------------------

/// A single JSONL event from `codex exec --json` stdout.
///
/// Uses a flat struct with `event_type` string + optional fields so that
/// unknown/future event types are silently tolerated.
#[derive(Debug, Deserialize)]
pub struct CodexEvent {
    #[serde(rename = "type")]
    pub event_type: String,

    // thread.started
    #[serde(default)]
    pub thread_id: Option<String>,

    // turn.completed
    #[serde(default)]
    pub usage: Option<CodexUsage>,

    // turn.failed / error
    #[serde(default)]
    pub error: Option<CodexError>,

    // item.started / item.updated / item.completed
    #[serde(default)]
    pub item: Option<CodexItem>,
}

#[derive(Debug, Deserialize)]
pub struct CodexUsage {
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
}

#[derive(Debug, Deserialize)]
pub struct CodexError {
    pub message: String,
}

// ---------------------------------------------------------------------------
// Thread items
// ---------------------------------------------------------------------------

/// A thread item carried by item.started / item.updated / item.completed events.
#[derive(Debug, Deserialize)]
pub struct CodexItem {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,

    // agent_message
    #[serde(default)]
    pub text: Option<String>,

    // command_execution
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub aggregated_output: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub status: Option<String>,
}

impl CodexItem {
    /// Extract text from an agent_message or reasoning item.
    pub fn agent_text(&self) -> Option<&str> {
        if self.item_type == "agent_message" || self.item_type == "reasoning" {
            self.text.as_deref()
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Launch command builder
// ---------------------------------------------------------------------------

/// Build the Codex exec argv for SDK (JSONL) mode.
///
/// `thread_id`: if provided, resumes an existing thread for multi-turn.
/// The final `-` instructs Codex to read the prompt from stdin.
pub fn codex_sdk_args(program: &str, thread_id: Option<&str>) -> (String, Vec<String>) {
    let model = std::env::var("BATTY_AGENT_MODEL").ok();
    let effort = std::env::var("BATTY_AGENT_EFFORT").ok();
    codex_sdk_args_with_options(program, thread_id, model.as_deref(), effort.as_deref())
}

pub fn codex_sdk_args_with_options(
    program: &str,
    thread_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> (String, Vec<String>) {
    let mut args = vec!["exec".to_string(), "--json".to_string()];
    if let Some(model) = model {
        args.extend(["-m".into(), model.into()]);
    }
    if let Some(effort) = effort {
        args.extend([
            "-c".into(),
            format!(
                "model_reasoning_effort={}",
                serde_json::to_string(effort).expect("string serialization")
            ),
        ]);
    }
    args.push("--dangerously-bypass-approvals-and-sandbox".into());
    if let Some(tid) = thread_id {
        args.extend(["resume".into(), tid.into()]);
    }
    args.push("-".into());
    (program.into(), args)
}

pub fn codex_sdk_command(program: &str, prompt: &str, thread_id: Option<&str>) -> String {
    let (program, mut args) = codex_sdk_args(program, thread_id);
    args.pop();
    args.push(prompt.into());
    let mut command = format!("exec {program}");
    for arg in args {
        command.push(' ');
        command.push_str(&crate::team::agent_options::shell_quote(&arg));
    }
    command
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thread_started() {
        let line = r#"{"type":"thread.started","thread_id":"abc-123"}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "thread.started");
        assert_eq!(evt.thread_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn parse_turn_started() {
        let line = r#"{"type":"turn.started"}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "turn.started");
    }

    #[test]
    fn parse_turn_completed() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":50,"output_tokens":30}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "turn.completed");
        let usage = evt.usage.unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 30);
    }

    #[test]
    fn parse_turn_failed() {
        let line = r#"{"type":"turn.failed","error":{"message":"rate limit"}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "turn.failed");
        assert_eq!(evt.error.unwrap().message, "rate limit");
    }

    #[test]
    fn parse_item_agent_message() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Hello world"}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "item.completed");
        let item = evt.item.unwrap();
        assert_eq!(item.item_type, "agent_message");
        assert_eq!(item.agent_text(), Some("Hello world"));
    }

    #[test]
    fn parse_item_command_execution() {
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"file.txt\n","exit_code":0,"status":"completed"}}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        let item = evt.item.unwrap();
        assert_eq!(item.item_type, "command_execution");
        assert_eq!(item.command.as_deref(), Some("ls"));
        assert_eq!(item.exit_code, Some(0));
        assert!(item.agent_text().is_none());
    }

    #[test]
    fn parse_error_event() {
        let line = r#"{"type":"error","message":"fatal"}"#;
        // The flat struct puts message at top level for the error event type;
        // but the Codex format uses an error object. Let's handle both.
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "error");
    }

    #[test]
    fn unknown_event_type_tolerated() {
        let line = r#"{"type":"future.event","some_field":42}"#;
        let evt: CodexEvent = serde_json::from_str(line).unwrap();
        assert_eq!(evt.event_type, "future.event");
    }

    #[test]
    fn codex_sdk_command_new_session() {
        let cmd = codex_sdk_command("codex", "fix the bug", None);
        assert!(cmd.contains("exec codex 'exec' '--json'"));
        assert!(!cmd.contains("-m "));
        assert!(!cmd.contains("model_reasoning_effort"));
        assert!(cmd.contains("--dangerously-bypass-approvals-and-sandbox"));
        assert!(cmd.contains("'fix the bug'"));
        assert!(!cmd.contains("resume"));
    }

    #[test]
    fn codex_sdk_command_resume() {
        let cmd = codex_sdk_command("codex", "next step", Some("tid-123"));
        assert!(!cmd.contains("-m "));
        assert!(!cmd.contains("model_reasoning_effort"));
        assert!(cmd.contains("'resume' 'tid-123'"));
        assert!(cmd.contains("'next step'"));
    }

    #[test]
    fn codex_sdk_command_escapes_quotes() {
        let cmd = codex_sdk_command("codex", "fix user's bug", None);
        assert!(cmd.contains("user'\\''s"));
    }

    #[test]
    fn explicit_codex_options_are_forwarded_without_backend_defaults() {
        let (_, args) =
            codex_sdk_args_with_options("codex", None, Some("custom/model"), Some("low"));
        assert!(args.windows(2).any(|pair| pair == ["-m", "custom/model"]));
        assert!(args.iter().any(|s| s == "model_reasoning_effort=\"low\""));
        let (_, unset) = codex_sdk_args_with_options("codex", None, None, None);
        assert!(!unset.contains(&"-m".to_string()) && !unset.contains(&"-c".to_string()));
    }

    #[test]
    fn codex_sdk_args_new_session() {
        let (program, args) = codex_sdk_args("codex", None);
        assert_eq!(program, "codex");
        assert_eq!(
            args,
            vec![
                "exec",
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "-"
            ]
        );
    }

    #[test]
    fn codex_sdk_args_resume() {
        let (program, args) = codex_sdk_args("codex", Some("tid-123"));
        assert_eq!(program, "codex");
        assert_eq!(
            args,
            vec![
                "exec",
                "--json",
                "--dangerously-bypass-approvals-and-sandbox",
                "resume",
                "tid-123",
                "-"
            ]
        );
    }
}
