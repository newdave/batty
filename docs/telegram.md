# Telegram

Batty can bridge a `user` role to a Telegram chat. The bot token comes from
`channel_config.bot_token` or `BATTY_TELEGRAM_BOT_TOKEN`; only numeric user IDs
in `allowed_user_ids` are accepted (an empty list denies everyone). See
`batty telegram` for guided setup.

## Messages sent while Batty is offline

Batty never replays chat messages that arrived while the daemon was down, so an
old `/merge` or `/assign` is not executed again after a restart. On the first
poll after startup Batty acknowledges the backlog and ignores it. If it skipped
anything, it replies once in each affected chat:

> Batty restarted; ignored N message(s) sent while it was offline. Resend any
> commands you still need.

Telegram returns only the newest pending update on that first poll, so `N` is a
lower bound when several messages piled up. Messages from users outside
`allowed_user_ids` are never counted or answered.

## Long replies

Replies longer than 4096 characters are split into several messages. If a send
fails part-way, a retry of the same text in the same chat resumes at the failed
chunk for up to 10 minutes after the first failure. After that, or once the
message has been fully delivered, an identical message is sent in full.
