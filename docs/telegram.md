# Telegram

Batty can bridge a `user` role to a Telegram chat. The bot token comes from
`channel_config.bot_token` or `BATTY_TELEGRAM_BOT_TOKEN`; only numeric user IDs
in `allowed_user_ids` are accepted (an empty list denies everyone). See
`batty telegram` for guided setup.

## Messages sent while Batty is offline

Batty never replays chat messages that arrived while the daemon was down, so an
old `/merge` or `/assign` is not executed again after a restart. On the first
poll after startup Batty reads the pending updates (`getUpdates` from offset 0,
up to 100 per page), counts them per chat, then acknowledges them and ignores
them. If it skipped anything, it replies once in each affected chat:

> Batty restarted; ignored N message(s) sent while it was offline. Resend any
> commands you still need.

`N` is the number of messages from authorized users in that chat, shown as
`100+` when it reaches 100. Messages from users outside `allowed_user_ids` are
never counted or answered. Telegram only keeps unacknowledged updates for about
24 hours, so older messages are not seen.

## Long replies

Replies longer than 4096 characters are split into several messages. If a send
fails part-way, a retry of the same text in the same chat resumes at the failed
chunk for up to 10 minutes after the first failure. After that, or once the
message has been fully delivered, an identical message is sent in full.
