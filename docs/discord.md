# Discord

The Discord integration uses a bot with three channels: `#commands` (inbound
`$` commands), `#events` and `#agents` (outbound embeds). Only user IDs in
`allowed_user_ids` can issue commands, and the bot needs the MESSAGE CONTENT
intent enabled in the Discord Developer Portal.

## Messages sent while Batty is offline

Batty never replays `#commands` messages that were posted while the daemon was
down, so an old `$merge` or `$assign` is not executed again after a restart. On
the first poll after startup Batty reads the 100 most recent messages in
`#commands`, counts the authorized ones, sets its cursor past them and ignores
them. If that skipped anything, Batty posts one reply in `#commands`:

> Batty restarted; ignored N message(s) sent while it was offline. Resend any
> commands you still need.

`N` counts authorized messages among those 100, shown as `100+` when it reaches
100; if the channel had more than 100 newer messages the count is a lower bound.
Messages from the bot itself and from users outside `allowed_user_ids` are not
counted.
