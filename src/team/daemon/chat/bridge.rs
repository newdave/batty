//! Shared chat bridge flow: poll a platform, route inbound text to roles or the
//! command dispatcher, deliver the user inbox, and track poll health.
//!
//! Platform specifics (client type, command grammar, reply routing, which
//! roles receive plain messages) live behind [`ChatPlatform`]. Authorization is
//! enforced by each client inside `poll`, so every [`ChatInbound`] is already
//! from an allowed sender.

use anyhow::{Error, Result};
use tracing::{debug, info, warn};

use super::super::background_tests::ChatReplyTarget;
use super::super::integrations::Integrations;
use super::super::*;
use super::TelegramCommand;
use crate::team::integration_health::{HealthTransition, PollHealth};

/// An authorized inbound chat message, platform-neutral.
pub(super) struct ChatInbound {
    pub(super) from_user_id: i64,
    pub(super) text: String,
    /// Where replies to this message go.
    pub(super) reply_to: ChatReplyTarget,
}

/// Inbound/outbound specifics of one chat platform.
pub(super) trait ChatPlatform: Sized {
    /// Integration name used in logs, events, and routing records.
    const NAME: &'static str;

    /// The daemon bot slot and poll-health tracker for this platform.
    fn slot(integrations: &mut Integrations) -> (&mut Option<Self>, &mut PollHealth);

    /// Fetch new messages from authorized senders.
    fn poll(&mut self) -> Result<Vec<ChatInbound>>;

    /// A configuration fault discovered by the last poll, if any.
    fn take_fault(&mut self) -> Option<String> {
        None
    }

    /// Messages ignored because they were sent while Batty was offline, as
    /// (where to reply, how many). Each backlog is returned once.
    fn take_skipped_backlog(&mut self) -> Vec<(ChatReplyTarget, usize)> {
        Vec::new()
    }

    fn log_poll_failure(error: &Error) {
        debug!(error = %error, "{} poll failed", Self::NAME);
    }

    /// Parse chat text as a command; `Ok(None)` means plain text to forward.
    fn parse_command(text: &str) -> Result<Option<TelegramCommand>>;

    /// Send `text` back to where `to` came from.
    fn send_reply(&self, to: &ChatReplyTarget, text: &str) -> Result<()>;

    /// Roles that receive plain (non-command) inbound text.
    fn inbox_targets(team_config: &TeamConfig) -> Vec<String>;
}

/// Reply text telling a user that `count` of their messages were skipped.
fn offline_backlog_notice(count: usize) -> String {
    format!(
        "Batty restarted; ignored {count} message(s) sent while it was offline. \
         Resend any commands you still need."
    )
}

/// Tell each chat with a skipped startup backlog, once.
fn notify_skipped_backlog<P: ChatPlatform>(bot: &mut P) {
    for (target, count) in bot.take_skipped_backlog() {
        if let Err(error) = bot.send_reply(&target, &offline_backlog_notice(count)) {
            warn!(error = %error, "failed to send {} offline-backlog notice", P::NAME);
        }
    }
}

impl TeamDaemon {
    /// Poll one platform and route what arrived.
    pub(super) fn poll_chat_platform<P: ChatPlatform>(&mut self) -> Result<()> {
        let (slot, health) = P::slot(&mut self.integrations);
        let Some(bot) = slot.as_mut() else {
            return Ok(());
        };

        let polled = bot.poll();
        let fault = if polled.is_ok() {
            notify_skipped_backlog(bot);
            bot.take_fault()
        } else {
            None
        };
        let transition = match &polled {
            Ok(_) => health.record_success(),
            Err(_) => health.record_failure(),
        };

        let messages = match polled {
            Ok(messages) => {
                if let Some(transition) = transition {
                    self.report_integration_health(P::NAME, transition, "");
                }
                messages
            }
            Err(error) => {
                P::log_poll_failure(&error);
                if let Some(transition) = transition {
                    self.report_integration_health(P::NAME, transition, &error.to_string());
                }
                return Ok(());
            }
        };

        if let Some(message) = fault {
            warn!(message = %message, "{} configuration fault", P::NAME);
            self.record_loop_step_error(&format!("process_{}_queue", P::NAME), &message);
        }

        if messages.is_empty() {
            return Ok(());
        }

        let root = inbox::inboxes_root(&self.config.project_root);
        let targets = P::inbox_targets(&self.config.team_config);

        for msg in messages {
            info!(
                from_user = msg.from_user_id,
                text_len = msg.text.len(),
                "{} inbound",
                P::NAME
            );

            if let Some(reply) = self.handle_chat_command::<P>(&msg) {
                if let Some(bot) = P::slot(&mut self.integrations).0.as_ref()
                    && let Err(error) = bot.send_reply(&msg.reply_to, &reply)
                {
                    warn!(error = %error, "failed to send {} reply", P::NAME);
                }
                continue;
            }

            for target in &targets {
                let inbox_msg = inbox::InboxMessage::new_send("human", target, &msg.text);
                if let Err(error) = inbox::deliver_to_inbox(&root, &inbox_msg) {
                    warn!(
                        to = %target,
                        error = %error,
                        "failed to deliver {} message to inbox",
                        P::NAME
                    );
                }
            }

            self.record_message_routed("human", P::NAME);
        }

        Ok(())
    }

    fn handle_chat_command<P: ChatPlatform>(&mut self, msg: &ChatInbound) -> Option<String> {
        let command = match P::parse_command(&msg.text) {
            Ok(Some(command)) => command,
            Ok(None) => return None,
            Err(error) => return Some(error.to_string()),
        };

        Some(
            match self.execute_chat_command(command, msg.reply_to.clone()) {
                Ok(reply) => reply,
                Err(error) => format!("Command failed: {error}"),
            },
        )
    }

    /// Warn and emit a team event when a chat integration poll health flips.
    pub(in crate::team::daemon) fn report_integration_health(
        &mut self,
        integration: &str,
        transition: HealthTransition,
        error: &str,
    ) {
        match transition {
            HealthTransition::BecameUnhealthy { failures } => {
                warn!(integration, failures, error, "chat integration unhealthy");
                self.emit_event(TeamEvent::integration_unhealthy(
                    integration,
                    failures,
                    error,
                ));
            }
            HealthTransition::Recovered { failures } => {
                info!(integration, failures, "chat integration recovered");
                self.emit_event(TeamEvent::integration_recovered(integration, failures));
            }
        }
    }

    pub(in crate::team::daemon) fn deliver_user_channel_inbox(&mut self) -> Result<()> {
        let root = inbox::inboxes_root(&self.config.project_root);
        let user_roles: Vec<String> = self
            .config
            .team_config
            .roles
            .iter()
            .filter(|role| role.role_type == RoleType::User)
            .map(|role| role.name.clone())
            .collect();

        for user_name in &user_roles {
            let messages = match inbox::pending_messages(&root, user_name) {
                Ok(msgs) => msgs,
                Err(error) => {
                    debug!(user = %user_name, error = %error, "failed to read user inbox");
                    continue;
                }
            };

            if messages.is_empty() {
                continue;
            }

            for msg in &messages {
                debug!(from = %msg.from, to = %user_name, id = %msg.id, "delivering to user channel");

                let formatted = format!("--- Message from {} ---\n{}", msg.from, msg.body);
                let send_result = match self.delivery.channels.get(user_name) {
                    Some(channel) => channel.send(&formatted),
                    None => {
                        debug!(user = %user_name, "no channel for user role");
                        break;
                    }
                };
                if let Err(error) = send_result {
                    // The channel logs real failures and backs off; leave the rest of
                    // this inbox for a later poll instead of hammering a broken channel.
                    debug!(to = %user_name, error = %error, "user channel send failed; deferring inbox");
                    break;
                }

                info!(from = %msg.from, to = %user_name, id = %msg.id, "delivered to user channel");
                let marked = inbox::mark_delivered(&root, user_name, &msg.id);
                self.record_message_routed(&msg.from, user_name);
                if let Err(error) = marked {
                    // The message was sent but stays pending. Stop here so this
                    // poll does not deliver further messages (and compound the
                    // duplicates); the channel recent-send dedup suppresses
                    // the resend on the next poll.
                    warn!(user = %user_name, id = %msg.id, error = %error, "failed to mark delivered; stopping delivery for this poll");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// A platform whose startup seed skipped a backlog, recording replies.
    struct FakePlatform {
        backlog: usize,
        polled_inbound: Vec<ChatInbound>,
        replies: RefCell<Vec<(ChatReplyTarget, String)>>,
    }

    impl FakePlatform {
        fn with_backlog(backlog: usize) -> Self {
            Self {
                backlog,
                polled_inbound: Vec::new(),
                replies: RefCell::new(Vec::new()),
            }
        }
    }

    impl ChatPlatform for FakePlatform {
        const NAME: &'static str = "fake";

        fn slot(_: &mut Integrations) -> (&mut Option<Self>, &mut PollHealth) {
            unreachable!("not used by notice tests")
        }

        fn poll(&mut self) -> Result<Vec<ChatInbound>> {
            // A seeding poll processes none of the backlog.
            Ok(std::mem::take(&mut self.polled_inbound))
        }

        fn take_skipped_backlog(&mut self) -> Vec<(ChatReplyTarget, usize)> {
            match std::mem::take(&mut self.backlog) {
                0 => Vec::new(),
                n => vec![(ChatReplyTarget::Discord, n)],
            }
        }

        fn parse_command(_: &str) -> Result<Option<TelegramCommand>> {
            Ok(None)
        }

        fn send_reply(&self, to: &ChatReplyTarget, text: &str) -> Result<()> {
            self.replies
                .borrow_mut()
                .push((to.clone(), text.to_string()));
            Ok(())
        }

        fn inbox_targets(_: &TeamConfig) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn three_message_backlog_gives_one_notice_and_processes_none() {
        let mut bot = FakePlatform::with_backlog(3);
        assert!(
            bot.poll().unwrap().is_empty(),
            "no backlog message is processed"
        );

        notify_skipped_backlog(&mut bot);
        notify_skipped_backlog(&mut bot);

        let replies = bot.replies.borrow();
        assert_eq!(replies.len(), 1, "exactly one notice");
        assert_eq!(replies[0].0, ChatReplyTarget::Discord);
        assert_eq!(
            replies[0].1,
            "Batty restarted; ignored 3 message(s) sent while it was offline. \
             Resend any commands you still need."
        );
    }

    #[test]
    fn empty_backlog_sends_no_notice() {
        let mut bot = FakePlatform::with_backlog(0);
        notify_skipped_backlog(&mut bot);
        assert!(bot.replies.borrow().is_empty());
    }
}
