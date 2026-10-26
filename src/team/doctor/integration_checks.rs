//! Connectivity checks for configured chat integrations (Telegram, Discord).

use anyhow::Result;

use super::util::check_line;
use super::{CheckLevel, CheckLine, TeamConfig};
use crate::team::config::RoleType;
use crate::team::discord::DiscordBot;
use crate::team::telegram;

/// Probe every configured chat integration (Telegram `getMe`, Discord
/// `users/@me`). Returns no lines when no user role has a channel configured.
pub(super) fn build_integration_checks(team_config: Option<&TeamConfig>) -> Vec<CheckLine> {
    build_integration_checks_with(
        team_config,
        &|token| telegram::validate_bot_token(token),
        &|bot| bot.validate_token().map(|identity| identity.username),
    )
}

fn build_integration_checks_with(
    team_config: Option<&TeamConfig>,
    telegram_probe: &dyn Fn(&str) -> Result<String>,
    discord_probe: &dyn Fn(&DiscordBot) -> Result<String>,
) -> Vec<CheckLine> {
    let Some(team_config) = team_config else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for role in team_config
        .roles
        .iter()
        .filter(|role| role.role_type == RoleType::User)
    {
        let (Some(channel), Some(config)) = (role.channel.as_deref(), role.channel_config.as_ref())
        else {
            continue;
        };
        match channel {
            "telegram" => {
                let token = config
                    .bot_token
                    .clone()
                    .or_else(|| std::env::var("BATTY_TELEGRAM_BOT_TOKEN").ok());
                match token {
                    Some(token) => lines.push(match telegram_probe(&token) {
                        Ok(username) => check_line(
                            CheckLevel::Pass,
                            format!("telegram ({}): bot @{username} reachable", role.name),
                        ),
                        Err(error) => check_line(
                            CheckLevel::Fail,
                            format!("telegram ({}): getMe failed: {error:#}", role.name),
                        ),
                    }),
                    None => lines.push(check_line(
                        CheckLevel::Warn,
                        format!(
                            "telegram ({}): no bot token; sends use CLI provider '{}' (no connectivity check)",
                            role.name, config.provider
                        ),
                    )),
                }
            }
            "discord" => match DiscordBot::from_config(config) {
                Some(bot) => lines.push(match discord_probe(&bot) {
                    Ok(username) => check_line(
                        CheckLevel::Pass,
                        format!("discord ({}): bot {username} reachable", role.name),
                    ),
                    Err(error) => check_line(
                        CheckLevel::Fail,
                        format!("discord ({}): users/@me failed: {error:#}", role.name),
                    ),
                }),
                None => lines.push(check_line(
                    CheckLevel::Warn,
                    format!(
                        "discord ({}): missing bot token or commands_channel_id; integration disabled",
                        role.name
                    ),
                )),
            },
            _ => {}
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::config::{ChannelConfig, RoleDef};
    use anyhow::anyhow;

    fn config_with(channel: &str, channel_config: ChannelConfig) -> TeamConfig {
        let yaml = "name: t\nroles: []\n";
        let mut config: TeamConfig = serde_yaml::from_str(yaml).unwrap();
        config.roles.push(RoleDef {
            name: "human".to_string(),
            role_type: RoleType::User,
            channel: Some(channel.to_string()),
            channel_config: Some(channel_config),
            ..Default::default()
        });
        config
    }

    fn channel_config(token: Option<&str>) -> ChannelConfig {
        ChannelConfig {
            target: "1".to_string(),
            provider: "openclaw".to_string(),
            bot_token: token.map(str::to_string),
            allowed_user_ids: vec![],
            events_channel_id: None,
            agents_channel_id: None,
            commands_channel_id: Some("99".to_string()),
            board_channel_id: None,
        }
    }

    #[test]
    fn no_team_config_yields_no_lines() {
        let lines = build_integration_checks_with(None, &|_| Ok("x".into()), &|_| Ok("x".into()));
        assert!(lines.is_empty());
    }

    #[test]
    fn telegram_probe_success_and_failure() {
        let config = config_with("telegram", channel_config(Some("tok")));
        let ok = build_integration_checks_with(
            Some(&config),
            &|_| Ok("batty_bot".into()),
            &|_| unreachable!(),
        );
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].level, CheckLevel::Pass);
        assert!(ok[0].message.contains("@batty_bot"));

        let bad = build_integration_checks_with(
            Some(&config),
            &|_| Err(anyhow!("401 Unauthorized")),
            &|_| unreachable!(),
        );
        assert_eq!(bad[0].level, CheckLevel::Fail);
        assert!(bad[0].message.contains("401 Unauthorized"));
    }

    #[test]
    fn discord_probe_runs_only_when_configured() {
        let config = config_with("discord", channel_config(Some("tok")));
        let ok = build_integration_checks_with(Some(&config), &|_| unreachable!(), &|_| {
            Ok("batty#1".into())
        });
        assert_eq!(ok[0].level, CheckLevel::Pass);

        let mut missing = channel_config(Some("tok"));
        missing.commands_channel_id = None;
        let config = config_with("discord", missing);
        let warn =
            build_integration_checks_with(Some(&config), &|_| unreachable!(), &|_| unreachable!());
        assert_eq!(warn[0].level, CheckLevel::Warn);
    }

    #[test]
    fn unknown_channel_is_skipped() {
        let config = config_with("slack", channel_config(Some("tok")));
        let lines =
            build_integration_checks_with(Some(&config), &|_| unreachable!(), &|_| unreachable!());
        assert!(lines.is_empty());
    }
}
