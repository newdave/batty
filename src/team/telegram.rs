//! Native Telegram Bot API client for batty.
//!
//! Provides a blocking HTTP client that sends messages and polls for updates
//! via the Telegram Bot API. Access control is enforced by numeric user IDs.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write as IoWrite};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use super::config::ChannelConfig;

/// An inbound message received from a Telegram user.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    pub from_user_id: i64,
    pub chat_id: i64,
    pub text: String,
}

/// Updates requested per `getUpdates` call (the Bot API maximum).
const UPDATES_PAGE_LIMIT: usize = 100;
/// Pages read while draining the startup backlog (bounds the first poll).
const MAX_BACKLOG_PAGES: usize = 20;

/// Blocking Telegram Bot API client.
pub struct TelegramBot {
    bot_token: String,
    allowed_user_ids: Vec<i64>,
    last_update_offset: i64,
    /// False until the first poll has acknowledged the startup backlog.
    seeded: bool,
    /// Authorized messages skipped by the startup seed, as (chat id, count),
    /// waiting to be reported once via `take_skipped_backlog`.
    skipped_backlog: Vec<(i64, usize)>,
    /// Chunks already delivered for multi-chunk messages whose send failed
    /// part-way, keyed by hash of (chat, text). A retry resumes after the last
    /// delivered chunk instead of re-sending the earlier ones. Entries expire
    /// `CHUNK_PROGRESS_TTL` after the first failed attempt, so a later,
    /// identical but unrelated message is sent in full.
    chunk_progress: Mutex<HashMap<u64, ChunkProgress>>,
}

/// Partial delivery of one multi-chunk message.
#[derive(Debug, Clone, Copy)]
struct ChunkProgress {
    delivered: usize,
    /// When the first failed attempt recorded progress; retries keep it.
    first_attempt: Instant,
}

/// Bound on remembered partially-sent messages.
const MAX_CHUNK_PROGRESS_ENTRIES: usize = 64;

/// How long a partial send stays resumable.
const CHUNK_PROGRESS_TTL: Duration = Duration::from_secs(600);

impl TelegramBot {
    /// Create a new `TelegramBot` with the given token and allowed user IDs.
    pub fn new(bot_token: String, allowed_user_ids: Vec<i64>) -> Self {
        Self {
            bot_token,
            allowed_user_ids,
            last_update_offset: 0,
            seeded: false,
            skipped_backlog: Vec::new(),
            chunk_progress: Mutex::new(HashMap::new()),
        }
    }

    /// Build a `TelegramBot` from a `ChannelConfig`.
    ///
    /// Returns `None` if no bot token is available — checks the config's
    /// `bot_token` field first, then falls back to the `BATTY_TELEGRAM_BOT_TOKEN`
    /// environment variable.
    pub fn from_config(config: &ChannelConfig) -> Option<Self> {
        let token = config
            .bot_token
            .clone()
            .or_else(|| std::env::var("BATTY_TELEGRAM_BOT_TOKEN").ok());

        token.map(|t| Self::new(t, config.allowed_user_ids.clone()))
    }

    /// Check whether a Telegram user ID is in the allowed list.
    ///
    /// An empty `allowed_user_ids` list denies everyone.
    pub fn is_authorized(&self, user_id: i64) -> bool {
        self.allowed_user_ids.contains(&user_id)
    }

    /// Maximum message length allowed by the Telegram Bot API.
    const MAX_MESSAGE_LEN: usize = 4096;

    /// Send a text message to a Telegram chat.
    ///
    /// Messages longer than 4096 characters are split into multiple messages.
    /// POST `https://api.telegram.org/bot{token}/sendMessage`
    ///
    /// If a later chunk fails, the chunks already delivered are remembered so a
    /// retry of the same message resumes after them rather than duplicating.
    pub fn send_message(&self, chat_id: &str, text: &str) -> Result<()> {
        self.send_chunks(chat_id, text, |chunk| {
            self.send_message_chunk(chat_id, chunk)
        })
    }

    fn send_chunks(
        &self,
        chat_id: &str,
        text: &str,
        send_chunk: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        self.send_chunks_at(Instant::now(), chat_id, text, send_chunk)
    }

    fn send_chunks_at(
        &self,
        now: Instant,
        chat_id: &str,
        text: &str,
        mut send_chunk: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        let chunks = split_message(text, Self::MAX_MESSAGE_LEN);
        let total_chunks = chunks.len();
        let progress_key = outbound_chunk_message_id(chat_id, text, 0, total_chunks);
        let previous = self.take_live_chunk_progress(now, progress_key);
        let start = previous.map_or(0, |p| p.delivered).min(total_chunks);

        for (index, chunk) in chunks.into_iter().enumerate().skip(start) {
            let chunk_id = outbound_chunk_message_id(chat_id, chunk, index, total_chunks);
            debug!(
                chat_id,
                chunk_index = index,
                total_chunks,
                chunk_id,
                "sending telegram message chunk"
            );
            if let Err(error) = send_chunk(chunk) {
                if index > 0 {
                    let first_attempt = previous.map_or(now, |p| p.first_attempt);
                    self.set_chunk_progress(
                        now,
                        progress_key,
                        ChunkProgress {
                            delivered: index,
                            first_attempt,
                        },
                    );
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Remove and return the resumable progress for `key`, dropping it when
    /// it is older than `CHUNK_PROGRESS_TTL`. The caller re-records progress
    /// if the send fails again, so a completed message leaves no entry.
    fn take_live_chunk_progress(&self, now: Instant, key: u64) -> Option<ChunkProgress> {
        let mut progress = self.chunk_progress.lock().ok()?;
        let entry = progress.remove(&key)?;
        (now.saturating_duration_since(entry.first_attempt) < CHUNK_PROGRESS_TTL).then_some(entry)
    }

    fn set_chunk_progress(&self, now: Instant, key: u64, entry: ChunkProgress) {
        if let Ok(mut progress) = self.chunk_progress.lock() {
            progress
                .retain(|_, p| now.saturating_duration_since(p.first_attempt) < CHUNK_PROGRESS_TTL);
            if progress.len() >= MAX_CHUNK_PROGRESS_ENTRIES {
                progress.clear();
            }
            progress.insert(key, entry);
        }
    }

    /// Send a single message chunk (must be <= 4096 chars).
    fn send_message_chunk(&self, chat_id: &str, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let body = serde_json::json!({
            "chat_id": chat_id,
            "text": text,
        });

        let resp = super::http::agent()
            .post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body.to_string());

        match resp {
            Ok(r) => {
                let status = r.status();
                let body: serde_json::Value = r
                    .into_json()
                    .context("failed to parse sendMessage response body")?;
                let message_id = parse_send_message_response(&body)?;
                debug!(status, message_id, "sendMessage accepted by Telegram API");
                Ok(())
            }
            Err(ureq::Error::Status(status, response)) => {
                let detail =
                    redact_token(&response.into_string().unwrap_or_default(), &self.bot_token);
                warn!(status, detail = %detail, "sendMessage failed");
                bail!("Telegram sendMessage failed with status {status}: {detail}");
            }
            Err(ureq::Error::Transport(error)) => {
                // ureq's error text includes the request URL, which embeds the token.
                let error = redact_token(&error.to_string(), &self.bot_token);
                warn!(error = %error, "sendMessage transport failed");
                bail!("Telegram sendMessage transport failed: {error}");
            }
        }
    }

    /// Poll the Telegram Bot API for new updates and return authorized inbound
    /// messages.
    ///
    /// GET `https://api.telegram.org/bot{token}/getUpdates?offset={}&timeout=0`
    ///
    /// Unauthorized messages are silently dropped (logged at debug level).
    pub fn poll_updates(&mut self) -> Result<Vec<InboundMessage>> {
        if !self.seeded {
            let token = self.bot_token.clone();
            return self.seed_from_backlog(|offset| Self::get_updates(&token, offset));
        }
        let body = Self::get_updates(&self.bot_token, self.last_update_offset)?;
        let (messages, new_offset) =
            parse_updates_response(&body, &self.allowed_user_ids, self.last_update_offset);
        Ok(self.advance_update_cursor(messages, new_offset))
    }

    fn get_updates(bot_token: &str, offset: i64) -> Result<serde_json::Value> {
        let url = format!(
            "https://api.telegram.org/bot{}/getUpdates?offset={offset}&limit={UPDATES_PAGE_LIMIT}&timeout=0",
            bot_token
        );
        match super::http::agent().get(&url).call() {
            Ok(r) => Ok(r.into_json()?),
            Err(e) => {
                let e = redact_token(&e.to_string(), bot_token);
                warn!(error = %e, "getUpdates failed");
                bail!("Telegram getUpdates failed: {e}");
            }
        }
    }

    /// First poll after startup: read the pending updates from offset 0 (the
    /// Bot API returns every unacknowledged update, oldest first, up to the
    /// limit), count the authorized messages, and acknowledge past the last
    /// one by advancing the offset. Nothing is processed, so commands handled
    /// by a previous run (and replayed by Telegram until acknowledged) are
    /// not executed again. A full page means more may be pending, so keep
    /// reading (bounded) until the backlog is drained.
    fn seed_from_backlog(
        &mut self,
        mut fetch: impl FnMut(i64) -> Result<serde_json::Value>,
    ) -> Result<Vec<InboundMessage>> {
        let mut skipped = Vec::new();
        let mut offset = 0;
        for _ in 0..MAX_BACKLOG_PAGES {
            let body = fetch(offset)?;
            let raw = body
                .get("result")
                .and_then(|r| r.as_array())
                .map_or(0, Vec::len);
            let (messages, new_offset) =
                parse_updates_response(&body, &self.allowed_user_ids, offset);
            skipped.extend(messages);
            let advanced = new_offset > offset;
            offset = new_offset;
            if raw < UPDATES_PAGE_LIMIT || !advanced {
                break;
            }
        }
        Ok(self.advance_update_cursor(skipped, offset))
    }

    /// Chats that sent messages while Batty was offline, with how many were
    /// skipped (the real count, which the bridge notice caps at "100+"). Returns each entry once; empty after the first call.
    pub fn take_skipped_backlog(&mut self) -> Vec<(i64, usize)> {
        std::mem::take(&mut self.skipped_backlog)
    }

    /// Advance the update offset and return the messages to process. The
    /// first poll after startup only seeds the offset and processes nothing.
    fn advance_update_cursor(
        &mut self,
        messages: Vec<InboundMessage>,
        new_offset: i64,
    ) -> Vec<InboundMessage> {
        self.last_update_offset = new_offset;
        if !self.seeded {
            self.seeded = true;
            if !messages.is_empty() {
                info!(
                    skipped = messages.len(),
                    "seeded Telegram update offset; ignoring messages sent before startup"
                );
            }
            for message in &messages {
                match self
                    .skipped_backlog
                    .iter_mut()
                    .find(|(chat_id, _)| *chat_id == message.chat_id)
                {
                    Some((_, count)) => *count += 1,
                    None => self.skipped_backlog.push((message.chat_id, 1)),
                }
            }
            return Vec::new();
        }
        messages
    }
}

fn parse_send_message_response(json: &serde_json::Value) -> Result<i64> {
    if json["ok"].as_bool() != Some(true) {
        return Err(anyhow!("Telegram API returned ok=false"));
    }

    json.get("result")
        .and_then(|result| result.get("message_id"))
        .and_then(|message_id| message_id.as_i64())
        .ok_or_else(|| anyhow!("Telegram sendMessage response missing result.message_id"))
}

fn outbound_chunk_message_id(
    chat_id: &str,
    chunk: &str,
    chunk_index: usize,
    total_chunks: usize,
) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    chat_id.hash(&mut hasher);
    chunk.hash(&mut hasher);
    chunk_index.hash(&mut hasher);
    total_chunks.hash(&mut hasher);
    hasher.finish()
}

/// Parse a Telegram `getUpdates` JSON response into authorized inbound messages.
///
/// Returns the list of messages and the updated offset (max `update_id` + 1).
fn parse_updates_response(
    json: &serde_json::Value,
    allowed: &[i64],
    current_offset: i64,
) -> (Vec<InboundMessage>, i64) {
    let mut messages = Vec::new();
    let mut new_offset = current_offset;

    let results = match json.get("result").and_then(|r| r.as_array()) {
        Some(arr) => arr,
        None => return (messages, new_offset),
    };

    for update in results {
        // Track offset regardless of message content
        if let Some(update_id) = update.get("update_id").and_then(|v| v.as_i64()) {
            if update_id + 1 > new_offset {
                new_offset = update_id + 1;
            }
        }

        let message = match update.get("message") {
            Some(m) => m,
            None => continue,
        };

        let from_id = message
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_i64());

        let chat_id = message
            .get("chat")
            .and_then(|c| c.get("id"))
            .and_then(|v| v.as_i64());

        let text = message
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());

        let (Some(from_id), Some(chat_id), Some(text)) = (from_id, chat_id, text) else {
            continue;
        };

        if !allowed.contains(&from_id) {
            debug!(from_id, "dropping message from unauthorized user");
            continue;
        }

        messages.push(InboundMessage {
            from_user_id: from_id,
            chat_id,
            text,
        });
    }

    (messages, new_offset)
}

/// Interactive Telegram bot setup wizard.
/// Called by `batty telegram` CLI command.
pub fn setup_telegram(project_root: &Path) -> Result<()> {
    let config_path = project_root
        .join(".batty")
        .join("team_config")
        .join("team.yaml");
    if !config_path.exists() {
        bail!(
            "no team config found at {}; run `batty init` first",
            config_path.display()
        );
    }

    println!("Telegram Bot Setup");
    println!("==================\n");

    // Step 1: Get and validate bot token
    println!("Step 1: Bot Token");
    println!("  1) Open Telegram and chat with @BotFather");
    println!("  2) Send /newbot (or /mybots to reuse an existing bot)");
    println!("  3) Copy the token (looks like 123456:ABC-DEF...)");
    println!();
    let bot_token = prompt_bot_token()?;

    // Step 2: Get and validate user ID
    println!("Step 2: Your Telegram User ID");
    println!("  Option A: DM @userinfobot — it replies with your numeric ID");
    println!("  Option B: DM @getidsbot");
    println!("  Option C: Call https://api.telegram.org/bot<TOKEN>/getUpdates");
    println!("            after sending your bot a message, and read message.from.id");
    println!();
    let user_id = prompt_user_id()?;

    // Step 3: Optional test message
    println!("Step 3: Test Message");
    println!("  IMPORTANT: You must DM your bot first (send /start in Telegram)");
    println!("  before the bot can message you. Telegram blocks bots from initiating");
    println!("  conversations with users who haven't messaged the bot.");
    println!();
    let send_test = prompt_yes_no("Send a test message? [Y/n]: ", true)?;
    if send_test {
        let bot = TelegramBot::new(bot_token.clone(), vec![user_id]);
        let chat_id = user_id.to_string();
        match bot.send_message(&chat_id, "Batty Telegram bridge configured!") {
            Ok(()) => println!("Test message sent. Check your Telegram.\n"),
            Err(e) => {
                println!("Warning: test message failed: {e}");
                println!("  Did you DM the bot first? Open your bot in Telegram and send /start,");
                println!("  then run `batty telegram` again.\n");
            }
        }
    }

    // Step 4: Update team.yaml
    update_team_yaml(&config_path, &bot_token, user_id)?;

    println!("Telegram configured successfully.");
    println!("Restart the daemon with: batty stop && batty start");

    Ok(())
}

/// Prompt for bot token and validate via getMe API.
fn prompt_bot_token() -> Result<String> {
    loop {
        let token = prompt("Enter your Telegram bot token (from @BotFather): ")?;
        if token.is_empty() {
            println!("Token cannot be empty. Try again.\n");
            continue;
        }

        match validate_bot_token(&token) {
            Ok(username) => {
                println!("Bot validated: @{username}\n");
                return Ok(token);
            }
            Err(e) => {
                println!("Validation failed: {e}");
                let retry = prompt_yes_no("Try again? [Y/n]: ", true)?;
                if !retry {
                    bail!("setup cancelled");
                }
            }
        }
    }
}

/// Replace every occurrence of the bot token in `text`. Telegram puts the
/// token in the request path, and ureq includes the URL in its error text.
fn redact_token(text: &str, token: &str) -> String {
    super::http::redact_secret(text, token)
}

/// Validate a bot token by calling the getMe API.
/// Returns the bot username on success.
pub(crate) fn validate_bot_token(token: &str) -> Result<String> {
    let url = format!("https://api.telegram.org/bot{token}/getMe");
    let resp: serde_json::Value = super::http::agent()
        .get(&url)
        .call()
        .map_err(|e| anyhow::anyhow!(redact_token(&e.to_string(), token)))?
        .into_json()?;

    if resp["ok"].as_bool() != Some(true) {
        bail!("Telegram API returned ok=false");
    }

    resp["result"]["username"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("missing username in getMe response"))
}

/// Prompt for numeric Telegram user ID.
fn prompt_user_id() -> Result<i64> {
    loop {
        let input = prompt("Enter your Telegram user ID (from @userinfobot): ")?;
        match input.trim().parse::<i64>() {
            Ok(id) if id > 0 => return Ok(id),
            _ => {
                println!("Invalid user ID — must be a positive number. Try again.\n");
            }
        }
    }
}

/// Read a line from stdin with a prompt.
fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

/// Prompt for yes/no with a default.
fn prompt_yes_no(msg: &str, default_yes: bool) -> Result<bool> {
    let input = prompt(msg)?;
    if input.is_empty() {
        return Ok(default_yes);
    }
    Ok(input.starts_with('y') || input.starts_with('Y'))
}

/// Parse a getMe API response and extract the bot username.
/// Exposed for testing.
pub fn parse_get_me_response(json: &serde_json::Value) -> Option<String> {
    if json["ok"].as_bool() != Some(true) {
        return None;
    }
    json["result"]["username"].as_str().map(|s| s.to_string())
}

/// Update team.yaml with Telegram credentials.
/// Uses serde_yaml round-trip (simple, loses comments but team templates have few).
fn update_team_yaml(path: &Path, bot_token: &str, user_id: i64) -> Result<()> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let user_id_str = user_id.to_string();

    // Find or create the user role
    let roles = doc
        .get_mut("roles")
        .and_then(|r| r.as_sequence_mut())
        .ok_or_else(|| anyhow::anyhow!("no 'roles' sequence in team.yaml"))?;

    let user_role = roles.iter_mut().find(|r| {
        r.get("role_type")
            .and_then(|v| v.as_str())
            .map(|s| s == "user")
            .unwrap_or(false)
    });

    if let Some(role) = user_role {
        // Update existing user role
        role["channel"] = serde_yaml::Value::String("telegram".into());

        // Ensure channel_config exists as a mapping
        if role.get("channel_config").is_none() {
            if let Some(role_map) = role.as_mapping_mut() {
                role_map.insert(
                    serde_yaml::Value::String("channel_config".into()),
                    serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
                );
            }
        }

        let cc = &mut role["channel_config"];

        if let Some(mapping) = cc.as_mapping_mut() {
            mapping.insert(
                serde_yaml::Value::String("target".into()),
                serde_yaml::Value::String(user_id_str),
            );
            mapping.insert(
                serde_yaml::Value::String("bot_token".into()),
                serde_yaml::Value::String(bot_token.into()),
            );
            mapping.insert(
                serde_yaml::Value::String("provider".into()),
                serde_yaml::Value::String("openclaw".into()),
            );
            // allowed_user_ids as a sequence of integers
            let ids = vec![serde_yaml::Value::Number(serde_yaml::Number::from(user_id))];
            mapping.insert(
                serde_yaml::Value::String("allowed_user_ids".into()),
                serde_yaml::Value::Sequence(ids),
            );
        }
    } else {
        // Append a new user role
        let mut new_role = serde_yaml::Mapping::new();
        new_role.insert("name".into(), "human".into());
        new_role.insert("role_type".into(), "user".into());
        new_role.insert("channel".into(), "telegram".into());

        let mut cc = serde_yaml::Mapping::new();
        cc.insert("target".into(), serde_yaml::Value::String(user_id_str));
        cc.insert(
            "bot_token".into(),
            serde_yaml::Value::String(bot_token.into()),
        );
        cc.insert("provider".into(), "openclaw".into());
        let ids = vec![serde_yaml::Value::Number(serde_yaml::Number::from(user_id))];
        cc.insert("allowed_user_ids".into(), serde_yaml::Value::Sequence(ids));
        new_role.insert("channel_config".into(), serde_yaml::Value::Mapping(cc));

        let talks_to = vec!["architect".into()];
        new_role.insert("talks_to".into(), serde_yaml::Value::Sequence(talks_to));

        roles.push(serde_yaml::Value::Mapping(new_role));
    }

    let output = serde_yaml::to_string(&doc)?;
    std::fs::write(path, &output).with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

/// Split a message into chunks of at most `max_len` bytes, never splitting
/// a UTF-8 character.
///
/// Tries to split on newline boundaries first, falling back to hard splits
/// when a single line exceeds `max_len`.
fn split_message(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_len {
            chunks.push(remaining);
            break;
        }

        // Never split inside a multi-byte UTF-8 character.
        let mut limit = max_len;
        while !remaining.is_char_boundary(limit) {
            limit -= 1;
        }
        if limit == 0 {
            // max_len is smaller than the first character; emit it whole.
            limit = remaining
                .chars()
                .next()
                .map_or(remaining.len(), char::len_utf8);
        }

        // Try to find a newline to split on within the limit
        let split_at = remaining[..limit]
            .rfind('\n')
            .map(|pos| pos + 1) // include the newline in the current chunk
            .unwrap_or(limit); // hard split if no newline found

        chunks.push(&remaining[..split_at]);
        remaining = &remaining[split_at..];
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_poll_seeds_offset_without_replaying_backlog() {
        let mut bot = TelegramBot::new("token".to_string(), vec![42]);
        let message = |text: &str| InboundMessage {
            from_user_id: 42,
            chat_id: 42,
            text: text.to_string(),
        };

        let first = bot.advance_update_cursor(vec![message("/merge 7")], 101);
        assert!(first.is_empty(), "startup backlog must not be processed");
        assert_eq!(bot.last_update_offset, 101);

        let second = bot.advance_update_cursor(vec![message("/status")], 102);
        assert_eq!(second.len(), 1);
        assert_eq!(bot.last_update_offset, 102);
    }

    fn update(id: i64, chat: i64, text: &str) -> serde_json::Value {
        serde_json::json!({"update_id": id, "message": {
            "from": {"id": 42}, "chat": {"id": chat}, "text": text}})
    }

    #[test]
    fn first_poll_counts_real_backlog_across_pages_and_acknowledges() {
        let mut bot = TelegramBot::new("token".to_string(), vec![42]);
        let mut offsets = Vec::new();
        let result = bot.seed_from_backlog(|offset| {
            offsets.push(offset);
            // Page 1 is full (100 updates, chat 7); page 2 holds 5 more.
            let (from, n) = if offset == 0 { (1, 100) } else { (101, 5) };
            Ok(serde_json::json!({"ok": true, "result":
                (from..from + n).map(|i| update(i, 7, "/status")).collect::<Vec<_>>()}))
        });
        assert!(result.unwrap().is_empty());
        assert_eq!(offsets, vec![0, 101], "must start at 0 and ack page one");
        assert_eq!(bot.last_update_offset, 106);
        assert_eq!(bot.take_skipped_backlog(), vec![(7, 105)]);
    }

    #[test]
    fn first_poll_with_empty_backlog_reports_nothing() {
        let mut bot = TelegramBot::new("token".to_string(), vec![42]);
        bot.seed_from_backlog(|_| Ok(serde_json::json!({"ok": true, "result": []})))
            .unwrap();
        assert!(bot.take_skipped_backlog().is_empty());
        assert_eq!(bot.last_update_offset, 0);
    }

    #[test]
    fn first_poll_records_skipped_backlog_per_chat_once() {
        let mut bot = TelegramBot::new("token".to_string(), vec![42]);
        let message = |chat_id: i64| InboundMessage {
            from_user_id: 42,
            chat_id,
            text: "/status".to_string(),
        };
        let first = bot.advance_update_cursor(vec![message(7), message(7), message(9)], 11);
        assert!(first.is_empty());
        assert_eq!(bot.take_skipped_backlog(), vec![(7, 2), (9, 1)]);
        assert!(bot.take_skipped_backlog().is_empty());

        bot.advance_update_cursor(vec![message(7)], 12);
        assert!(bot.take_skipped_backlog().is_empty());
    }

    #[test]
    fn redact_token_removes_token_from_ureq_error_text() {
        let token = "123456:ABC-secret";
        let text = format!(
            "https://api.telegram.org/bot{token}/getUpdates?offset=0: Dns Failed: no such host"
        );
        let redacted = redact_token(&text, token);
        assert!(!redacted.contains(token));
        assert!(redacted.contains("bot<redacted>/getUpdates"));
        assert_eq!(redact_token("unchanged", ""), "unchanged");
    }

    #[test]
    fn split_message_does_not_split_multibyte_chars() {
        // "é" is two bytes; a 5-byte limit lands mid-character.
        let text = "aéééé";
        let chunks = split_message(text, 4);
        assert_eq!(chunks.concat(), text);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 4));

        let emoji = "🎉🎉🎉";
        let chunks = split_message(emoji, 5);
        assert_eq!(chunks, vec!["🎉", "🎉", "🎉"]);

        let chunks = split_message(emoji, 2);
        assert_eq!(chunks.concat(), emoji);
    }

    #[test]
    fn new_constructor_sets_fields() {
        let bot = TelegramBot::new("token123".into(), vec![111, 222]);
        assert_eq!(bot.bot_token, "token123");
        assert_eq!(bot.allowed_user_ids, vec![111, 222]);
        assert_eq!(bot.last_update_offset, 0);
    }

    #[test]
    fn is_authorized_allows_listed_id() {
        let bot = TelegramBot::new("t".into(), vec![100, 200, 300]);
        assert!(bot.is_authorized(100));
        assert!(bot.is_authorized(200));
        assert!(bot.is_authorized(300));
    }

    #[test]
    fn is_authorized_rejects_unlisted_id() {
        let bot = TelegramBot::new("t".into(), vec![100, 200]);
        assert!(!bot.is_authorized(999));
    }

    #[test]
    fn is_authorized_empty_list_denies_all() {
        let bot = TelegramBot::new("t".into(), vec![]);
        assert!(!bot.is_authorized(100));
        assert!(!bot.is_authorized(0));
    }

    #[test]
    fn from_config_returns_none_without_token() {
        // Neither config nor env var has a token.
        // We cannot safely unset an env var in edition 2024, so we just ensure
        // the config field is None. If the env var happens to be set externally,
        // from_config would return Some — that's correct behaviour. We only
        // assert None when we're confident the env var is absent.
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "telegram".into(),
            bot_token: None,
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };

        // If the env var is not set, from_config must return None.
        if std::env::var("BATTY_TELEGRAM_BOT_TOKEN").is_err() {
            assert!(TelegramBot::from_config(&config).is_none());
        }
    }

    #[test]
    fn from_config_returns_some_with_config_token() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "telegram".into(),
            bot_token: Some("bot-tok-from-config".into()),
            allowed_user_ids: vec![42],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };

        let bot = TelegramBot::from_config(&config).expect("should return Some");
        assert_eq!(bot.bot_token, "bot-tok-from-config");
        assert_eq!(bot.allowed_user_ids, vec![42]);
    }

    #[test]
    fn from_config_prefers_config_token_over_env() {
        // Even if the env var is set, the config token takes precedence.
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "telegram".into(),
            bot_token: Some("from-config".into()),
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };

        let bot = TelegramBot::from_config(&config).unwrap();
        assert_eq!(bot.bot_token, "from-config");
    }

    #[test]
    fn parse_updates_authorized_message() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": [
                {
                    "update_id": 123456,
                    "message": {
                        "from": {"id": 12345678},
                        "chat": {"id": 12345678},
                        "text": "hello from telegram"
                    }
                }
            ]
        });

        let allowed = vec![12345678_i64];
        let (msgs, new_offset) = parse_updates_response(&json, &allowed, 0);

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from_user_id, 12345678);
        assert_eq!(msgs[0].chat_id, 12345678);
        assert_eq!(msgs[0].text, "hello from telegram");
        assert_eq!(new_offset, 123457); // update_id + 1
    }

    #[test]
    fn parse_updates_unauthorized_user_filtered() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": [
                {
                    "update_id": 100,
                    "message": {
                        "from": {"id": 99999},
                        "chat": {"id": 99999},
                        "text": "sneaky message"
                    }
                }
            ]
        });

        let allowed = vec![12345678_i64];
        let (msgs, new_offset) = parse_updates_response(&json, &allowed, 0);

        assert!(msgs.is_empty(), "unauthorized message should be filtered");
        assert_eq!(new_offset, 101); // offset still advances
    }

    #[test]
    fn parse_updates_empty_result() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": []
        });

        let (msgs, new_offset) = parse_updates_response(&json, &[42], 50);
        assert!(msgs.is_empty());
        assert_eq!(new_offset, 50); // unchanged
    }

    #[test]
    fn parse_updates_multiple_messages_mixed_auth() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": [
                {
                    "update_id": 200,
                    "message": {
                        "from": {"id": 111},
                        "chat": {"id": 111},
                        "text": "authorized msg"
                    }
                },
                {
                    "update_id": 201,
                    "message": {
                        "from": {"id": 999},
                        "chat": {"id": 999},
                        "text": "unauthorized msg"
                    }
                },
                {
                    "update_id": 202,
                    "message": {
                        "from": {"id": 222},
                        "chat": {"id": 222},
                        "text": "also authorized"
                    }
                }
            ]
        });

        let allowed = vec![111_i64, 222];
        let (msgs, new_offset) = parse_updates_response(&json, &allowed, 0);

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text, "authorized msg");
        assert_eq!(msgs[1].text, "also authorized");
        assert_eq!(new_offset, 203);
    }

    #[test]
    fn parse_updates_skips_non_message_updates() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": [
                {
                    "update_id": 300,
                    "edited_message": {
                        "from": {"id": 42},
                        "chat": {"id": 42},
                        "text": "edited"
                    }
                }
            ]
        });

        let (msgs, new_offset) = parse_updates_response(&json, &[42], 0);
        assert!(msgs.is_empty());
        assert_eq!(new_offset, 301);
    }

    #[test]
    fn parse_updates_skips_message_without_text() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": [
                {
                    "update_id": 400,
                    "message": {
                        "from": {"id": 42},
                        "chat": {"id": 42}
                    }
                }
            ]
        });

        let (msgs, new_offset) = parse_updates_response(&json, &[42], 0);
        assert!(msgs.is_empty());
        assert_eq!(new_offset, 401);
    }

    #[test]
    fn parse_get_me_valid_response() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": {
                "id": 123456789,
                "is_bot": true,
                "first_name": "Test Bot",
                "username": "test_bot"
            }
        });
        assert_eq!(parse_get_me_response(&json), Some("test_bot".to_string()));
    }

    #[test]
    fn parse_get_me_invalid_response() {
        let json: serde_json::Value = serde_json::json!({
            "ok": false,
            "description": "Not Found"
        });
        assert_eq!(parse_get_me_response(&json), None);
    }

    #[test]
    fn parse_send_message_response_requires_api_acceptance() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 42
            }
        });

        assert_eq!(parse_send_message_response(&json).unwrap(), 42);
    }

    #[test]
    fn parse_send_message_response_rejects_missing_confirmation() {
        let json: serde_json::Value = serde_json::json!({
            "ok": true,
            "result": {}
        });

        assert!(
            parse_send_message_response(&json)
                .unwrap_err()
                .to_string()
                .contains("message_id")
        );
    }

    #[test]
    fn update_team_yaml_updates_existing_user_role() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(
            &path,
            r#"
name: test-team
roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      target: "placeholder"
      provider: openclaw
    talks_to: [architect]
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();

        update_team_yaml(&path, "123:abc-token", 99887766).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("123:abc-token"));
        assert!(content.contains("99887766"));
    }

    #[test]
    fn update_team_yaml_creates_user_role_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("team.yaml");
        std::fs::write(
            &path,
            r#"
name: test-team
roles:
  - name: architect
    role_type: architect
    agent: claude
"#,
        )
        .unwrap();

        update_team_yaml(&path, "456:xyz-token", 11223344).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("456:xyz-token"));
        assert!(content.contains("11223344"));
        assert!(content.contains("user"));
        assert!(content.contains("human"));
    }

    #[test]
    fn setup_telegram_bails_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        let result = setup_telegram(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("batty init"));
    }

    #[test]
    fn split_message_short_text_returns_single_chunk() {
        let chunks = split_message("hello", 4096);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_message_exact_limit_returns_single_chunk() {
        let text = "a".repeat(4096);
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 4096);
    }

    #[test]
    fn split_message_splits_on_newline() {
        let line = "a".repeat(2000);
        let text = format!("{line}\n{line}\n{line}");
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.len(), 2);
        // First chunk: two lines with newlines = 2001 + 2001 = 4002
        assert!(chunks[0].len() <= 4096);
        assert!(chunks[1].len() <= 4096);
        // Reassembled text matches original
        let reassembled: String = chunks.iter().copied().collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn split_message_hard_splits_long_line() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 904);
    }

    #[test]
    fn split_message_multiple_chunks() {
        let text = "a".repeat(10000);
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), 4096);
        assert_eq!(chunks[1].len(), 4096);
        assert_eq!(chunks[2].len(), 1808);
        let reassembled: String = chunks.iter().copied().collect();
        assert_eq!(reassembled, text);
    }

    #[test]
    fn split_message_empty_text() {
        let chunks = split_message("", 4096);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn outbound_chunk_message_id_changes_per_chunk() {
        let text = format!("{}\n{}", "a".repeat(4096), "b".repeat(200));
        let chunks = split_message(&text, 4096);
        assert_eq!(chunks.len(), 2);

        let first = outbound_chunk_message_id("12345", chunks[0], 0, chunks.len());
        let second = outbound_chunk_message_id("12345", chunks[1], 1, chunks.len());

        assert_ne!(first, second);
    }

    #[test]
    fn send_chunks_resumes_after_last_delivered_chunk() {
        let bot = TelegramBot::new("tok".into(), vec![]);
        let text = "a".repeat(10_000); // 3 chunks
        let mut sent = Vec::new();
        let result = bot.send_chunks("1", &text, |chunk| {
            if sent.len() == 2 {
                anyhow::bail!("boom");
            }
            sent.push(chunk.len());
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(sent.len(), 2);

        let mut resumed = Vec::new();
        bot.send_chunks("1", &text, |chunk| {
            resumed.push(chunk.len());
            Ok(())
        })
        .unwrap();
        // Only the third chunk is resent.
        assert_eq!(resumed, vec![10_000 - 2 * 4096]);

        // Progress is cleared after success: a fresh send starts from chunk 0.
        let mut again = 0;
        bot.send_chunks("1", &text, |_| {
            again += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(again, 3);
    }

    #[test]
    fn identical_message_after_completed_send_delivers_all_chunks() {
        let bot = TelegramBot::new("tok".into(), vec![]);
        let text = "a".repeat(10_000); // 3 chunks
        let mut calls = 0;
        // Fail the 3rd chunk once, then complete on retry.
        let _ = bot.send_chunks("1", &text, |_| {
            calls += 1;
            if calls == 3 {
                anyhow::bail!("boom");
            }
            Ok(())
        });
        bot.send_chunks("1", &text, |_| Ok(())).unwrap();

        let mut again = 0;
        bot.send_chunks("1", &text, |_| {
            again += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(again, 3, "repeat of a completed message is sent in full");
    }

    #[test]
    fn stale_partial_progress_expires_so_identical_message_is_sent_in_full() {
        let bot = TelegramBot::new("tok".into(), vec![]);
        let text = "a".repeat(10_000);
        let start = Instant::now();
        let mut n = 0;
        let _ = bot.send_chunks_at(start, "1", &text, |_| {
            n += 1;
            if n == 3 {
                anyhow::bail!("boom")
            } else {
                Ok(())
            }
        });

        // Within the TTL the retry resumes at the failed chunk.
        let mut resumed = 0;
        let _ = bot.send_chunks_at(start + Duration::from_secs(60), "1", &text, |_| {
            resumed += 1;
            anyhow::bail!("still down")
        });
        assert_eq!(resumed, 1, "resumes at chunk 3");

        // Retries keep the first-attempt time, so the entry expires 10 minutes
        // after the first failure, not after the latest retry.
        let mut full = 0;
        bot.send_chunks_at(start + Duration::from_secs(601), "1", &text, |_| {
            full += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(full, 3);
    }

    #[test]
    fn send_chunks_first_chunk_failure_retries_from_start() {
        let bot = TelegramBot::new("tok".into(), vec![]);
        let text = "a".repeat(5000);
        assert!(bot.send_chunks("1", &text, |_| anyhow::bail!("x")).is_err());
        let mut count = 0;
        bot.send_chunks("1", &text, |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn send_chunks_progress_is_per_chat_and_text() {
        let bot = TelegramBot::new("tok".into(), vec![]);
        let text = "a".repeat(5000);
        let mut n = 0;
        let _ = bot.send_chunks("1", &text, |_| {
            n += 1;
            if n == 2 { anyhow::bail!("x") } else { Ok(()) }
        });
        let mut other = 0;
        bot.send_chunks("2", &text, |_| {
            other += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(other, 2);
    }
}
