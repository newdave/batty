//! External communication channels for user roles.
//!
//! The `user` role type communicates via channels (Telegram, Slack, etc.)
//! instead of tmux panes. Each channel provider is a CLI tool that the
//! daemon invokes for outbound messages.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::config::ChannelConfig;
use super::discord::DiscordBot;
use super::errors::DeliveryError;
use super::telegram::TelegramBot;

const TELEGRAM_DEDUP_TTL: Duration = Duration::from_secs(300);
const TELEGRAM_DEDUP_CAPACITY: usize = 512;
const SEND_BACKOFF_TRANSIENT_BASE: Duration = Duration::from_secs(30);
const SEND_BACKOFF_PERMANENT_BASE: Duration = Duration::from_secs(300);
const SEND_BACKOFF_MAX: Duration = Duration::from_secs(1800);

/// Exponential backoff after failed sends, so a broken channel (bad chat id,
/// missing token, provider error) is not retried on every daemon poll.
#[derive(Debug, Default)]
struct SendBackoff {
    state: Mutex<Option<(u32, Instant)>>,
}

impl SendBackoff {
    /// Returns the remaining wait if a retry is not yet due.
    fn remaining(&self, now: Instant) -> Option<Duration> {
        let state = self.state.lock().unwrap();
        state
            .and_then(|(_, retry_at)| retry_at.checked_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    fn record_failure(&self, now: Instant, transient: bool) -> Duration {
        let mut state = self.state.lock().unwrap();
        let failures = state.map_or(0, |(failures, _)| failures).saturating_add(1);
        let base = if transient {
            SEND_BACKOFF_TRANSIENT_BASE
        } else {
            SEND_BACKOFF_PERMANENT_BASE
        };
        let delay = base
            .saturating_mul(1u32 << (failures - 1).min(16))
            .min(SEND_BACKOFF_MAX);
        *state = Some((failures, now + delay));
        delay
    }

    fn record_success(&self) {
        *self.state.lock().unwrap() = None;
    }

    /// Gate a send attempt: skip while backing off, and update the backoff
    /// from the attempt's outcome.
    fn run(
        &self,
        recipient: &str,
        send: impl FnOnce() -> std::result::Result<(), DeliveryError>,
    ) -> std::result::Result<(), DeliveryError> {
        let now = Instant::now();
        if let Some(remaining) = self.remaining(now) {
            return Err(DeliveryError::ChannelBackoff {
                recipient: recipient.to_string(),
                retry_in_secs: remaining.as_secs().max(1),
            });
        }
        let result = send();
        match &result {
            Ok(()) => self.record_success(),
            Err(error) => {
                let delay = self.record_failure(now, error.is_transient());
                warn!(
                    recipient,
                    error = %error,
                    retry_in_secs = delay.as_secs(),
                    "channel send failed; backing off"
                );
            }
        }
        result
    }
}

#[derive(Debug)]
struct RecentTelegramSends {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<VecDeque<(u64, Instant)>>,
}

impl RecentTelegramSends {
    fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    fn prune_expired(&self, now: Instant, entries: &mut VecDeque<(u64, Instant)>) {
        while entries
            .front()
            .is_some_and(|(_, sent_at)| now.duration_since(*sent_at) > self.ttl)
        {
            entries.pop_front();
        }
        while entries.len() > self.capacity {
            entries.pop_front();
        }
    }

    fn contains_recent(&self, message_id: u64) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        self.prune_expired(now, &mut entries);
        entries.iter().any(|(id, _)| *id == message_id)
    }

    fn record(&self, message_id: u64) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        self.prune_expired(now, &mut entries);
        entries.push_back((message_id, now));
        self.prune_expired(now, &mut entries);
    }
}

fn telegram_message_id(target: &str, message: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut hasher);
    message.hash(&mut hasher);
    hasher.finish()
}

/// Trait for outbound message delivery to external channels.
pub trait Channel: Send + Sync {
    /// Send a text message to the channel destination.
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError>;
    /// Channel type identifier (e.g., "telegram").
    fn channel_type(&self) -> &str;
}

/// Telegram channel via openclaw (or any CLI provider).
pub struct TelegramChannel {
    target: String,
    provider: String,
    recent_sends: RecentTelegramSends,
    backoff: SendBackoff,
    /// Unix seconds when the provider last rejected `--channel`/`--target` and
    /// the legacy `--to` form worked (0 = never). Later sends use the legacy
    /// form until `legacy_reprobe_after` has passed, then try the modern form
    /// once more so an upgraded provider is picked up without a restart.
    legacy_since: AtomicU64,
    legacy_reprobe_after: Duration,
}

impl TelegramChannel {
    pub fn new(target: String, provider: String) -> Self {
        Self::with_dedup_settings(
            target,
            provider,
            TELEGRAM_DEDUP_TTL,
            TELEGRAM_DEDUP_CAPACITY,
        )
    }

    pub fn from_config(config: &ChannelConfig) -> Self {
        Self::new(config.target.clone(), config.provider.clone())
    }

    fn with_dedup_settings(
        target: String,
        provider: String,
        ttl: Duration,
        capacity: usize,
    ) -> Self {
        Self {
            target,
            provider,
            recent_sends: RecentTelegramSends::new(ttl, capacity),
            backoff: SendBackoff::default(),
            legacy_since: AtomicU64::new(0),
            legacy_reprobe_after: LEGACY_REPROBE_INTERVAL,
        }
    }
}

impl Channel for TelegramChannel {
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
        let message_id = telegram_message_id(&self.target, message);
        if self.recent_sends.contains_recent(message_id) {
            debug!(target = %self.target, message_id, "suppressing duplicate telegram message");
            return Ok(());
        }

        self.backoff
            .run(&self.target, || self.send_via_provider(message, message_id))
    }

    fn channel_type(&self) -> &str {
        "telegram"
    }
}

impl TelegramChannel {
    fn send_via_provider(
        &self,
        message: &str,
        message_id: u64,
    ) -> std::result::Result<(), DeliveryError> {
        debug!(target = %self.target, provider = %self.provider, len = message.len(), "sending via telegram channel");

        let run = |legacy: bool| {
            let args: Vec<&str> = if legacy {
                provider_send_args_legacy(&self.target, message).to_vec()
            } else {
                provider_send_args(&self.target, message).to_vec()
            };
            std::process::Command::new(&self.provider)
                .args(args)
                .output()
        };

        let now = unix_now_secs();
        let since = self.legacy_since.load(Ordering::Relaxed);
        let legacy = since != 0 && now.saturating_sub(since) < self.legacy_reprobe_after.as_secs();
        let mut output = run(legacy);
        if !legacy
            && let Ok(out) = &output
            && !out.status.success()
            && is_unknown_modern_flag(&String::from_utf8_lossy(&out.stderr))
        {
            debug!(provider = %self.provider, "provider rejected --target; retrying with legacy --to form");
            let retry = run(true);
            if matches!(&retry, Ok(out) if out.status.success()) {
                self.legacy_since.store(now.max(1), Ordering::Relaxed);
            }
            output = retry;
        }

        match output {
            Ok(out) if out.status.success() => {
                self.recent_sends.record(message_id);
                debug!("telegram message sent successfully");
                Ok(())
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                debug!(status = ?out.status, stderr = %stderr, "telegram send failed");
                Err(DeliveryError::ChannelSend {
                    recipient: self.target.clone(),
                    detail: stderr.to_string(),
                })
            }
            Err(e) => {
                debug!(error = %e, provider = %self.provider, "failed to execute channel provider");
                Err(DeliveryError::ProviderExec {
                    provider: self.provider.clone(),
                    source: e,
                })
            }
        }
    }
}

/// Arguments for `<provider> message send` (openclaw CLI contract).
fn provider_send_args<'a>(target: &'a str, message: &'a str) -> [&'a str; 8] {
    [
        "message",
        "send",
        "--channel",
        "telegram",
        "--target",
        target,
        "--message",
        message,
    ]
}

/// Legacy openclaw contract (`--to <target>` and no `--channel`).
fn provider_send_args_legacy<'a>(target: &'a str, message: &'a str) -> [&'a str; 6] {
    ["message", "send", "--to", target, "--message", message]
}

/// How long the legacy `--to` form is remembered before the modern form is
/// probed again.
const LEGACY_REPROBE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// True when stderr rejects `--channel` or `--target`: it names one of those
/// flags together with a rejection word ("unknown", "unrecognized",
/// "unexpected" or "invalid"), whatever argument parser the provider uses.
fn is_unknown_modern_flag(stderr: &str) -> bool {
    let text = stderr.to_ascii_lowercase();
    let rejected = ["unknown", "unrecognized", "unexpected", "invalid"]
        .iter()
        .any(|word| text.contains(word));
    rejected
        && ["--channel", "--target"]
            .iter()
            .any(|flag| text.contains(flag))
}

/// Native Telegram channel using the Bot API directly (no CLI provider).
pub struct NativeTelegramChannel {
    bot: TelegramBot,
    target: String,
    recent_sends: RecentTelegramSends,
    backoff: SendBackoff,
}

impl NativeTelegramChannel {
    pub fn new(bot: TelegramBot, target: String) -> Self {
        Self::with_dedup_settings(target, bot, TELEGRAM_DEDUP_TTL, TELEGRAM_DEDUP_CAPACITY)
    }

    /// Build from a `ChannelConfig`, returning `None` if no bot token is available.
    pub fn from_config(config: &ChannelConfig) -> Option<Self> {
        TelegramBot::from_config(config).map(|bot| Self::new(bot, config.target.clone()))
    }

    fn with_dedup_settings(
        target: String,
        bot: TelegramBot,
        ttl: Duration,
        capacity: usize,
    ) -> Self {
        Self {
            bot,
            target,
            recent_sends: RecentTelegramSends::new(ttl, capacity),
            backoff: SendBackoff::default(),
        }
    }
}

impl Channel for NativeTelegramChannel {
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
        let message_id = telegram_message_id(&self.target, message);
        if self.recent_sends.contains_recent(message_id) {
            debug!(
                target = %self.target,
                message_id,
                "suppressing duplicate native telegram message"
            );
            return Ok(());
        }

        self.backoff.run(&self.target, || {
            debug!(target = %self.target, len = message.len(), "sending via native telegram channel");
            self.bot
                .send_message(&self.target, message)
                .map(|_| {
                    self.recent_sends.record(message_id);
                })
                .map_err(|error| DeliveryError::ChannelSend {
                    recipient: self.target.clone(),
                    detail: error.to_string(),
                })
        })
    }

    fn channel_type(&self) -> &str {
        "telegram-native"
    }
}

/// Native Discord channel using the Bot API directly.
pub struct DiscordChannel {
    bot: DiscordBot,
    channel_id: String,
}

impl DiscordChannel {
    pub fn new(bot: DiscordBot, channel_id: String) -> Self {
        Self { bot, channel_id }
    }

    pub fn from_config(config: &ChannelConfig) -> Option<Self> {
        let channel_id = config
            .commands_channel_id
            .clone()
            .or_else(|| config.events_channel_id.clone())?;
        DiscordBot::from_config(config).map(|bot| Self::new(bot, channel_id))
    }
}

impl Channel for DiscordChannel {
    fn send(&self, message: &str) -> std::result::Result<(), DeliveryError> {
        self.bot
            .send_formatted_message(&self.channel_id, message)
            .map_err(|error| DeliveryError::ChannelSend {
                recipient: self.channel_id.clone(),
                detail: error.to_string(),
            })
    }

    fn channel_type(&self) -> &str {
        "discord"
    }
}

/// Create a channel from config fields.
pub fn channel_from_config(
    channel_type: &str,
    config: &ChannelConfig,
) -> std::result::Result<Box<dyn Channel>, DeliveryError> {
    match channel_type {
        "telegram" => {
            if let Some(native) = NativeTelegramChannel::from_config(config) {
                Ok(Box::new(native))
            } else {
                info!(
                    provider = %config.provider,
                    "telegram channel has no bot_token or BATTY_TELEGRAM_BOT_TOKEN; using CLI provider"
                );
                Ok(Box::new(TelegramChannel::from_config(config)))
            }
        }
        "discord" => DiscordChannel::from_config(config)
            .map(|channel| Box::new(channel) as Box<dyn Channel>)
            .ok_or_else(|| DeliveryError::UnsupportedChannel {
                channel_type: "discord".to_string(),
            }),
        other => Err(DeliveryError::UnsupportedChannel {
            channel_type: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn telegram_channel_type() {
        let ch = TelegramChannel::new("12345".into(), "openclaw".into());
        assert_eq!(ch.channel_type(), "telegram");
    }

    #[test]
    fn native_telegram_channel_type() {
        let bot = TelegramBot::new("test-token".into(), vec![]);
        let ch = NativeTelegramChannel::new(bot, "12345".into());
        assert_eq!(ch.channel_type(), "telegram-native");
    }

    #[test]
    fn discord_channel_type() {
        let bot = DiscordBot::new("test-token".into(), vec![42], "67890".into());
        let ch = DiscordChannel::new(bot, "67890".into());
        assert_eq!(ch.channel_type(), "discord");
    }

    #[test]
    fn channel_from_config_telegram() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
            bot_token: None,
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };
        // Without bot_token (and assuming env var is not set), falls back to CLI channel.
        if std::env::var("BATTY_TELEGRAM_BOT_TOKEN").is_err() {
            let ch = channel_from_config("telegram", &config).unwrap();
            assert_eq!(ch.channel_type(), "telegram");
        }
    }

    #[test]
    fn channel_from_config_telegram_with_bot_token() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
            bot_token: Some("test-bot-token".into()),
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };
        let ch = channel_from_config("telegram", &config).unwrap();
        assert_eq!(ch.channel_type(), "telegram-native");
    }

    #[test]
    fn channel_from_config_telegram_without_bot_token() {
        let config = ChannelConfig {
            target: "12345".into(),
            provider: "openclaw".into(),
            bot_token: None,
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };
        // Only assert CLI fallback when the env var is also absent.
        if std::env::var("BATTY_TELEGRAM_BOT_TOKEN").is_err() {
            let ch = channel_from_config("telegram", &config).unwrap();
            assert_eq!(ch.channel_type(), "telegram");
        }
    }

    #[test]
    fn channel_from_config_unknown_type() {
        let config = ChannelConfig {
            target: "x".into(),
            provider: "x".into(),
            bot_token: None,
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: None,
            board_channel_id: None,
        };
        match channel_from_config("slack", &config) {
            Err(e) => assert!(e.to_string().contains("unsupported")),
            Ok(_) => panic!("expected error for unsupported channel"),
        }
    }

    #[test]
    fn channel_from_config_discord() {
        let config = ChannelConfig {
            target: String::new(),
            provider: String::new(),
            bot_token: Some("discord-token".into()),
            allowed_user_ids: vec![42],
            events_channel_id: Some("100".into()),
            agents_channel_id: Some("200".into()),
            commands_channel_id: Some("300".into()),
            board_channel_id: None,
        };
        let ch = channel_from_config("discord", &config).unwrap();
        assert_eq!(ch.channel_type(), "discord");
    }

    #[test]
    fn telegram_send_fails_gracefully_with_missing_provider() {
        let ch = TelegramChannel::new("12345".into(), "/nonexistent/binary".into());
        let result = ch.send("hello");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to execute")
        );
    }

    #[test]
    fn telegram_message_id_changes_with_target_and_body() {
        let first = telegram_message_id("12345", "hello");
        let second = telegram_message_id("12345", "hello again");
        let third = telegram_message_id("67890", "hello");
        assert_ne!(first, second);
        assert_ne!(first, third);
    }

    #[test]
    fn telegram_recent_sends_respects_ttl() {
        let cache = RecentTelegramSends::new(Duration::from_millis(50), 16);
        let id = telegram_message_id("12345", "hello");
        assert!(!cache.contains_recent(id));
        cache.record(id);
        assert!(cache.contains_recent(id));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!cache.contains_recent(id));
    }

    #[test]
    fn telegram_channel_suppresses_duplicate_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let script_path = tmp.path().join("fake-provider.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
                log_path.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        fs::set_permissions(&script_path, perms).unwrap();

        let ch = TelegramChannel::with_dedup_settings(
            "12345".into(),
            script_path.display().to_string(),
            Duration::from_secs(60),
            16,
        );
        ch.send("hello").unwrap();
        ch.send("hello").unwrap();

        let lines = fs::read_to_string(&log_path).unwrap();
        assert_eq!(lines.lines().count(), 1);
    }

    #[test]
    fn telegram_channel_allows_unique_messages_and_retries_after_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let script_path = tmp.path().join("fake-provider.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\n",
                log_path.display()
            ),
        )
        .unwrap();
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o755);
        }
        fs::set_permissions(&script_path, perms).unwrap();

        let ch = TelegramChannel::with_dedup_settings(
            "12345".into(),
            script_path.display().to_string(),
            Duration::from_millis(5),
            16,
        );
        ch.send("first").unwrap();
        ch.send("second").unwrap();
        std::thread::sleep(Duration::from_millis(10));
        ch.send("first").unwrap();

        let lines = fs::read_to_string(&log_path).unwrap();
        assert_eq!(lines.lines().count(), 3);
    }

    fn write_fake_provider(dir: &std::path::Path, log_path: &std::path::Path, exit: i32) -> String {
        let script_path = dir.join("fake-provider.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit {exit}\n",
                log_path.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        script_path.display().to_string()
    }

    #[test]
    fn provider_send_args_use_openclaw_target_flag() {
        let args = provider_send_args("12345", "hi");
        assert_eq!(
            args,
            [
                "message",
                "send",
                "--channel",
                "telegram",
                "--target",
                "12345",
                "--message",
                "hi"
            ]
        );
        assert!(!args.contains(&"--to"));
    }

    /// Fake provider that rejects `--target` like an old openclaw would.
    fn write_legacy_only_provider(dir: &std::path::Path, log_path: &std::path::Path) -> String {
        let script_path = dir.join("legacy-provider.sh");
        fs::write(
            &script_path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$*\" in\n  *--target*|*--channel*) echo \"error: unknown option '--target'\" >&2; exit 1;;\nesac\nexit 0\n",
                log_path.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        script_path.display().to_string()
    }

    #[test]
    fn telegram_send_falls_back_to_legacy_to_flag_and_remembers_it() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let provider = write_legacy_only_provider(tmp.path(), &log_path);
        let ch = TelegramChannel::new("12345".into(), provider);

        ch.send("one").unwrap();
        ch.send("two").unwrap();

        let log = fs::read_to_string(&log_path).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines,
            [
                "message send --channel telegram --target 12345 --message one",
                "message send --to 12345 --message one",
                "message send --to 12345 --message two",
            ],
            "second send must skip the modern form"
        );
    }

    #[test]
    fn telegram_send_does_not_retry_on_unrelated_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let provider = write_fake_provider(tmp.path(), &log_path, 1);
        let ch = TelegramChannel::new("12345".into(), provider);
        assert!(ch.send("x").is_err());
        assert_eq!(fs::read_to_string(&log_path).unwrap().lines().count(), 1);
    }

    #[test]
    fn unknown_flag_detection_matches_channel_and_target_only() {
        assert!(is_unknown_modern_flag("error: unknown option '--target'"));
        assert!(is_unknown_modern_flag(
            "error: unknown option '--channel'\n"
        ));
        assert!(!is_unknown_modern_flag("error: unknown option '--to'"));
        assert!(!is_unknown_modern_flag("network unreachable"));
        assert!(is_unknown_modern_flag(
            "error: unrecognized arguments: --target 1"
        ));
        assert!(is_unknown_modern_flag(
            "Error: Unexpected argument '--channel' found"
        ));
        assert!(is_unknown_modern_flag("invalid option --target"));
        assert!(!is_unknown_modern_flag("unknown chat id 12345"));
        assert!(!is_unknown_modern_flag("unknown option '--to'"));
    }

    #[test]
    fn telegram_send_reprobes_modern_form_after_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let provider = write_legacy_only_provider(tmp.path(), &log_path);
        let mut ch = TelegramChannel::new("12345".into(), provider);
        ch.legacy_reprobe_after = Duration::ZERO;

        ch.send("one").unwrap();
        ch.send("two").unwrap();

        let log = fs::read_to_string(&log_path).unwrap();
        let modern_attempts = log.lines().filter(|l| l.contains("--target")).count();
        assert_eq!(modern_attempts, 2, "modern form must be re-probed:\n{log}");
    }

    #[test]
    fn telegram_send_passes_target_flag_to_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let provider = write_fake_provider(tmp.path(), &log_path, 0);

        TelegramChannel::new("12345".into(), provider)
            .send("hello")
            .unwrap();

        let line = fs::read_to_string(&log_path).unwrap();
        assert_eq!(
            line.trim(),
            "message send --channel telegram --target 12345 --message hello"
        );
    }

    #[test]
    fn telegram_send_backs_off_after_failure_without_reinvoking_provider() {
        let tmp = tempfile::tempdir().unwrap();
        let log_path = tmp.path().join("provider.log");
        let provider = write_fake_provider(tmp.path(), &log_path, 1);
        let ch = TelegramChannel::new("12345".into(), provider);

        let first = ch.send("one").unwrap_err();
        assert!(matches!(first, DeliveryError::ChannelSend { .. }));
        let second = ch.send("two").unwrap_err();
        assert!(matches!(second, DeliveryError::ChannelBackoff { .. }));

        let lines = fs::read_to_string(&log_path).unwrap();
        assert_eq!(
            lines.lines().count(),
            1,
            "provider must not run while backing off"
        );
    }

    #[test]
    fn send_backoff_grows_exponentially_and_caps() {
        let backoff = SendBackoff::default();
        let now = Instant::now();
        assert_eq!(backoff.record_failure(now, true), Duration::from_secs(30));
        assert_eq!(backoff.record_failure(now, true), Duration::from_secs(60));
        assert_eq!(backoff.record_failure(now, true), Duration::from_secs(120));
        for _ in 0..20 {
            backoff.record_failure(now, true);
        }
        assert_eq!(backoff.record_failure(now, true), SEND_BACKOFF_MAX);
    }

    #[test]
    fn send_backoff_uses_longer_base_for_permanent_errors() {
        let backoff = SendBackoff::default();
        let now = Instant::now();
        assert_eq!(backoff.record_failure(now, false), Duration::from_secs(300));
        assert!(backoff.remaining(now).is_some());
        assert!(backoff.remaining(now + Duration::from_secs(301)).is_none());
    }

    #[test]
    fn send_backoff_resets_after_success() {
        let backoff = SendBackoff::default();
        backoff.record_failure(Instant::now(), true);
        backoff.record_success();
        assert!(backoff.remaining(Instant::now()).is_none());
        assert_eq!(
            backoff.record_failure(Instant::now(), true),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn channel_backoff_error_is_not_retried_in_process() {
        let error = DeliveryError::ChannelBackoff {
            recipient: "12345".into(),
            retry_in_secs: 30,
        };
        assert!(!error.is_transient());
    }

    #[test]
    fn telegram_chat_not_found_is_permanent() {
        let error = DeliveryError::ChannelSend {
            recipient: "12345".into(),
            detail: r#"Telegram sendMessage failed with status 400: {"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#.into(),
        };
        assert!(!error.is_transient());
    }
}
