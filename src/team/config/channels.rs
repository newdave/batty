//! Validation of per-role chat channel configuration.
//!
//! Unknown channels and missing targets are errors (the daemon could never
//! deliver). Missing credentials are warnings: the daemon degrades to a
//! disabled integration instead of refusing to start.

use std::path::{Path, PathBuf};

use super::{RoleType, TeamConfig};

/// Channel names understood by `comms::channel_from_config`.
pub const KNOWN_CHANNELS: &[&str] = &["telegram", "discord"];

const TELEGRAM_TOKEN_ENV: &str = "BATTY_TELEGRAM_BOT_TOKEN";
const DISCORD_TOKEN_ENV: &str = "BATTY_DISCORD_BOT_TOKEN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelCheckLevel {
    Pass,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelCheck {
    pub name: String,
    pub level: ChannelCheckLevel,
    pub detail: String,
}

impl ChannelCheck {
    fn new(role: &str, aspect: &str, level: ChannelCheckLevel, detail: String) -> Self {
        Self {
            name: format!("channel_{aspect}:{role}"),
            level,
            detail,
        }
    }
}

/// Find `program` on PATH (or accept an existing explicit path).
fn find_binary(program: &str) -> Option<PathBuf> {
    let explicit = Path::new(program);
    if explicit.components().count() > 1 {
        return explicit.is_file().then(|| explicit.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

fn env_token_present(var: &str) -> bool {
    std::env::var(var).is_ok_and(|value| !value.trim().is_empty())
}

impl TeamConfig {
    /// Check every role that configures a channel.
    pub fn channel_checks(&self) -> Vec<ChannelCheck> {
        self.channel_checks_with(&find_binary, &env_token_present)
    }

    fn channel_checks_with(
        &self,
        find_binary: &dyn Fn(&str) -> Option<PathBuf>,
        env_present: &dyn Fn(&str) -> bool,
    ) -> Vec<ChannelCheck> {
        use ChannelCheckLevel::{Error, Pass, Warn};

        let mut checks = Vec::new();
        for role in &self.roles {
            let Some(channel) = role.channel.as_deref() else {
                continue;
            };
            let name = role.name.as_str();
            if !KNOWN_CHANNELS.contains(&channel) {
                checks.push(ChannelCheck::new(
                    name,
                    "type",
                    Error,
                    format!(
                        "role '{name}' uses unknown channel '{channel}'; supported: {}",
                        KNOWN_CHANNELS.join(", ")
                    ),
                ));
                continue;
            }
            if role.role_type != RoleType::User {
                checks.push(ChannelCheck::new(
                    name,
                    "type",
                    Warn,
                    format!("role '{name}' sets channel '{channel}' but is not a user role; it is ignored"),
                ));
                continue;
            }
            let config = role.channel_config.as_ref();
            let token_in_config = config
                .and_then(|c| c.bot_token.as_deref())
                .is_some_and(|t| !t.trim().is_empty());

            match channel {
                "telegram" => {
                    let target_empty = config.is_none_or(|c| c.target.trim().is_empty());
                    if target_empty {
                        checks.push(ChannelCheck::new(
                            name,
                            "target",
                            Error,
                            format!("role '{name}' telegram channel has an empty target (chat id)"),
                        ));
                    }
                    let provider = config.map(|c| c.provider.trim()).unwrap_or_default();
                    if token_in_config {
                        checks.push(ChannelCheck::new(
                            name,
                            "credentials",
                            Pass,
                            format!("role '{name}' telegram uses the native Bot API (bot_token in config)"),
                        ));
                    } else if env_present(TELEGRAM_TOKEN_ENV) {
                        checks.push(ChannelCheck::new(
                            name,
                            "credentials",
                            Pass,
                            format!("role '{name}' telegram uses the native Bot API (token from {TELEGRAM_TOKEN_ENV})"),
                        ));
                    } else if provider.is_empty() {
                        checks.push(ChannelCheck::new(
                            name,
                            "credentials",
                            Warn,
                            format!("role '{name}' telegram has no bot_token, no {TELEGRAM_TOKEN_ENV} and no provider; messages cannot be sent"),
                        ));
                    } else {
                        match find_binary(provider) {
                            Some(path) => checks.push(ChannelCheck::new(
                                name,
                                "credentials",
                                Pass,
                                format!("role '{name}' telegram has no bot token; will use CLI provider '{provider}' at {}", path.display()),
                            )),
                            None => checks.push(ChannelCheck::new(
                                name,
                                "credentials",
                                Warn,
                                format!("role '{name}' telegram has no bot token and provider '{provider}' was not found on PATH; messages cannot be sent"),
                            )),
                        }
                    }
                    if config.is_some_and(|c| c.allowed_user_ids.is_empty()) {
                        checks.push(ChannelCheck::new(
                            name,
                            "access",
                            Warn,
                            format!("role '{name}' telegram has no allowed_user_ids; inbound messages are ignored"),
                        ));
                    }
                }
                _ => {
                    // discord: sends and polls through the Bot API only.
                    let mut missing = Vec::new();
                    if !token_in_config && !env_present(DISCORD_TOKEN_ENV) {
                        missing.push(format!("bot_token (or {DISCORD_TOKEN_ENV})"));
                    }
                    let has_id = |id: Option<&String>| id.is_some_and(|v| !v.trim().is_empty());
                    if !has_id(config.and_then(|c| c.commands_channel_id.as_ref())) {
                        missing.push("commands_channel_id".to_string());
                    }
                    if missing.is_empty() {
                        checks.push(ChannelCheck::new(
                            name,
                            "credentials",
                            Pass,
                            format!(
                                "role '{name}' discord has a bot token and commands_channel_id"
                            ),
                        ));
                    } else {
                        checks.push(ChannelCheck::new(
                            name,
                            "credentials",
                            Warn,
                            format!(
                                "role '{name}' discord is missing {}; integration will be disabled",
                                missing.join(", ")
                            ),
                        ));
                    }
                    if config.is_none_or(|c| c.allowed_user_ids.is_empty()) {
                        checks.push(ChannelCheck::new(
                            name,
                            "access",
                            Warn,
                            format!("role '{name}' discord has no allowed_user_ids; commands are ignored"),
                        ));
                    }
                }
            }
        }
        checks
    }

    /// First channel configuration error, if any.
    pub fn channel_error(&self) -> Option<String> {
        self.channel_checks()
            .into_iter()
            .find(|check| check.level == ChannelCheckLevel::Error)
            .map(|check| check.detail)
    }

    /// Warning messages for channel configuration (missing credentials etc.).
    pub fn channel_warnings(&self) -> Vec<String> {
        self.channel_checks()
            .into_iter()
            .filter(|check| check.level == ChannelCheckLevel::Warn)
            .map(|check| check.detail)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::{ChannelConfig, RoleDef};

    fn team(channel: Option<&str>, cc: Option<ChannelConfig>) -> TeamConfig {
        let mut config: TeamConfig = serde_yaml::from_str("name: t\nroles: []\n").unwrap();
        config.roles.push(RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            channel: channel.map(str::to_string),
            channel_config: cc,
            ..Default::default()
        });
        config
    }

    fn cc(target: &str, provider: &str, token: Option<&str>) -> ChannelConfig {
        ChannelConfig {
            target: target.to_string(),
            provider: provider.to_string(),
            bot_token: token.map(str::to_string),
            allowed_user_ids: vec![1],
            commands_channel_id: Some("9".to_string()),
            ..Default::default()
        }
    }

    fn run(config: &TeamConfig, bin: Option<&str>, env: bool) -> Vec<ChannelCheck> {
        config.channel_checks_with(&|_| bin.map(PathBuf::from), &|_| env)
    }

    fn levels(checks: &[ChannelCheck]) -> Vec<ChannelCheckLevel> {
        checks.iter().map(|c| c.level).collect()
    }

    #[test]
    fn no_channel_yields_no_checks() {
        assert!(run(&team(None, None), None, false).is_empty());
    }

    #[test]
    fn unknown_channel_is_error() {
        let checks = run(&team(Some("slack"), None), None, false);
        assert_eq!(levels(&checks), vec![ChannelCheckLevel::Error]);
        assert!(checks[0].detail.contains("slack"));
        assert!(checks[0].detail.contains("telegram, discord"));
    }

    #[test]
    fn telegram_empty_target_is_error_even_without_config() {
        let checks = run(&team(Some("telegram"), None), None, false);
        assert!(checks.iter().any(|c| c.level == ChannelCheckLevel::Error));
        let checks = run(
            &team(Some("telegram"), Some(cc(" ", "openclaw", Some("t")))),
            None,
            false,
        );
        assert!(checks.iter().any(|c| c.level == ChannelCheckLevel::Error));
    }

    #[test]
    fn telegram_token_in_config_or_env_passes() {
        let checks = run(
            &team(Some("telegram"), Some(cc("1", "", Some("tok")))),
            None,
            false,
        );
        assert_eq!(levels(&checks), vec![ChannelCheckLevel::Pass]);
        assert!(checks[0].detail.contains("config"));

        let checks = run(&team(Some("telegram"), Some(cc("1", "", None))), None, true);
        assert_eq!(levels(&checks), vec![ChannelCheckLevel::Pass]);
        assert!(checks[0].detail.contains(TELEGRAM_TOKEN_ENV));
    }

    #[test]
    fn telegram_without_token_reports_provider_path_or_warns() {
        let config = team(Some("telegram"), Some(cc("1", "openclaw", None)));
        let found = run(&config, Some("/usr/local/bin/openclaw"), false);
        assert_eq!(levels(&found), vec![ChannelCheckLevel::Pass]);
        assert!(found[0].detail.contains("/usr/local/bin/openclaw"));

        let missing = run(&config, None, false);
        assert_eq!(levels(&missing), vec![ChannelCheckLevel::Warn]);
        assert!(missing[0].detail.contains("not found on PATH"));

        let no_provider = run(
            &team(Some("telegram"), Some(cc("1", "", None))),
            None,
            false,
        );
        assert_eq!(levels(&no_provider), vec![ChannelCheckLevel::Warn]);
    }

    #[test]
    fn discord_requires_token_and_channel_id_as_warnings() {
        let ok = run(
            &team(Some("discord"), Some(cc("", "", Some("tok")))),
            None,
            false,
        );
        assert_eq!(levels(&ok), vec![ChannelCheckLevel::Pass]);

        let mut no_channel = cc("", "", Some("tok"));
        no_channel.commands_channel_id = None;
        let checks = run(&team(Some("discord"), Some(no_channel)), None, false);
        assert_eq!(levels(&checks), vec![ChannelCheckLevel::Warn]);
        assert!(checks[0].detail.contains("commands_channel_id"));

        let checks = run(&team(Some("discord"), Some(cc("", "", None))), None, false);
        assert!(checks[0].detail.contains("bot_token"));
        assert_eq!(checks[0].level, ChannelCheckLevel::Warn);
    }

    #[test]
    fn channel_error_and_warnings_split_by_level() {
        let config = team(Some("telegram"), Some(cc("", "openclaw", None)));
        let error = config.channel_error();
        assert!(error.is_some_and(|e| e.contains("empty target")));
    }

    #[test]
    fn non_user_role_with_channel_is_warned() {
        let mut config = team(Some("telegram"), Some(cc("1", "", Some("t"))));
        config.roles[0].role_type = RoleType::Engineer;
        let checks = run(&config, None, false);
        assert_eq!(levels(&checks), vec![ChannelCheckLevel::Warn]);
    }

    #[test]
    fn find_binary_accepts_explicit_existing_path_only() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("provider");
        std::fs::write(&file, "").unwrap();
        assert_eq!(find_binary(file.to_str().unwrap()), Some(file));
        assert_eq!(find_binary("/definitely/not/here/provider"), None);
    }

    fn parse(yaml: &str) -> TeamConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    const BASE: &str = "name: t\nagent: claude\nroles:\n  - name: architect\n    role_type: architect\n    instances: 1\n  - name: human\n    role_type: user\n    channel: CHANNEL\n    talks_to: [architect]\n    channel_config:\n      target: TARGET\n      provider: openclaw\n      bot_token: tok\n";

    #[test]
    fn validate_rejects_unknown_channel_and_empty_telegram_target() {
        let unknown = parse(&BASE.replace("CHANNEL", "slack").replace("TARGET", "\"1\""));
        let error = unknown.validate().unwrap_err().to_string();
        assert!(error.contains("unknown channel 'slack'"), "{error}");

        let empty = parse(
            &BASE
                .replace("CHANNEL", "telegram")
                .replace("TARGET", "\"\""),
        );
        let error = empty.validate().unwrap_err().to_string();
        assert!(error.contains("empty target"), "{error}");

        let ok = parse(
            &BASE
                .replace("CHANNEL", "telegram")
                .replace("TARGET", "\"1\""),
        );
        ok.validate().unwrap();
    }

    #[test]
    fn validate_verbose_reports_channel_failures_and_warnings_without_failing() {
        let unknown = parse(&BASE.replace("CHANNEL", "slack").replace("TARGET", "\"1\""));
        assert!(
            unknown
                .validate_verbose()
                .iter()
                .any(|c| c.name == "channel_type:human" && !c.passed)
        );

        // Discord without allowed_user_ids/commands_channel_id only warns.
        let discord = parse(&BASE.replace("CHANNEL", "discord").replace("TARGET", "\"\""));
        let checks = discord.validate_verbose();
        let channel: Vec<_> = checks
            .iter()
            .filter(|c| c.name.starts_with("channel_"))
            .collect();
        assert!(!channel.is_empty());
        assert!(channel.iter().all(|c| c.passed));
        assert!(channel.iter().any(|c| c.detail.starts_with("WARN:")));
        discord.validate().unwrap();
        assert!(!discord.channel_warnings().is_empty());
    }
}
