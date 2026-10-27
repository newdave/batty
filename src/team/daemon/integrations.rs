//! Chat integrations polled by the daemon (Discord and Telegram bots) and the
//! health and cursor state that goes with them.

use tracing::{info, warn};

use super::background_tests::ChatReplyTarget;
use crate::team::discord::DiscordBot;
use crate::team::integration_health::PollHealth;
use crate::team::telegram::TelegramBot;

pub(in crate::team) struct Integrations {
    pub(in crate::team) discord_bot: Option<DiscordBot>,
    /// Number of events from the event log already mirrored to Discord.
    pub(in crate::team) discord_event_cursor: usize,
    pub(in crate::team) telegram_bot: Option<TelegramBot>,
    /// Consecutive-failure tracking for the Telegram / Discord pollers.
    pub(in crate::team) telegram_health: PollHealth,
    pub(in crate::team) discord_health: PollHealth,
}

impl Integrations {
    pub(in crate::team) fn new(
        discord_bot: Option<DiscordBot>,
        telegram_bot: Option<TelegramBot>,
        discord_event_cursor: usize,
    ) -> Self {
        Self {
            discord_bot,
            discord_event_cursor,
            telegram_bot,
            telegram_health: PollHealth::default(),
            discord_health: PollHealth::default(),
        }
    }

    /// Send the result of a background chat command back to where it came from.
    pub(in crate::team) fn send_chat_reply(&self, target: &ChatReplyTarget, reply: &str) {
        match target {
            ChatReplyTarget::None => {
                info!(reply = %reply, "background chat command finished");
            }
            ChatReplyTarget::Telegram { chat_id } => {
                if let Some(bot) = self.telegram_bot.as_ref()
                    && let Err(error) = bot.send_message(&chat_id.to_string(), reply)
                {
                    warn!(chat_id, error = %error, "failed to send telegram reply");
                }
            }
            ChatReplyTarget::Discord => {
                if let Some(bot) = self.discord_bot.as_ref()
                    && let Err(error) = bot.send_command_reply(reply)
                {
                    warn!(error = %error, "failed to send discord command reply");
                }
            }
        }
    }
}
