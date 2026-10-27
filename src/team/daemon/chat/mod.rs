//! Chat integrations (Telegram, Discord) driven by the daemon poll loop.
//!
//! `commands` holds the command dispatcher shared by both bridges.

mod commands;
pub(super) mod discord;
pub(super) mod telegram;

pub(super) use commands::TelegramCommand;
