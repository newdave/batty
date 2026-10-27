# Migrating to 0.12

This guide covers changes you may need to act on when upgrading from batty 0.11.x
(including upstream `battysh/batty`). The full list is in the
[changelog](../CHANGELOG.md).

## Upgrade steps

1. Stop running teams with `batty stop` before installing the new binary, and
   don't let old and new binaries merge in the same project at the same time.
   An older binary's empty `merge.lock` is treated as held for up to an hour,
   so a mixed setup can stall merges rather than corrupt them.
2. Install the new binary and run `batty validate` in each project. Fix any
   errors it reports about channels; review its warnings about unused keys.
3. Start the team with `batty start`. The tmux layout is unchanged (tiled
   panes) unless you opt into another mode (see [tmux layouts](tmux-layouts.md)).

## Behaviour changes to check

### tmux layout

Layouts still default to tiled panes (`layout.mode: panes`). Window numbers now
start at 1. Large teams can opt into a dashboard plus one window per member:

```yaml
layout:
  mode: windows   # or auto: windows only for more than four agents
```

Batty's function keys (F1–F12) are off by default (`layout.keys: false`). If you
turn them on, tmux's default mouse bindings stop working in batty sessions until
a later release fixes the key table.

### `batty status`

The default view is a summary plus a compact table. Use `batty status --wide`
for every column; when stdout is not a terminal, `--wide` prints full rows with
no fitting or truncation. `batty status --watch` exits with a non-zero status on
the first error when stdout is not a terminal (on a terminal it keeps retrying
and shows the error). Scripts should use `batty status --json`, which is
unchanged.

### `batty init`

`batty init --agent X` keeps the template's comments and any valid `model` and
`effort` pins, and reports the pins it drops. This applies to `--agent` only:
choosing an agent through the interactive prompts rewrites the file and drops
comments.

### `batty config --json`

The output keeps upstream's top-level keys (`team`, `roles`, `members`,
`board`, `standup`, `automation`, `workflow`) and adds `schema_version: 2` and a
full `config` object. `batty config` now exits non-zero when there is no team
config, so scripts that relied on exit code 0 must handle that.

### Git timeouts

Every git command batty runs now has a timeout: 120 seconds by default, 600
seconds for slow commands (fetch, push, commit, merge, rebase, checkout, reset,
stash, add, status, clean, diff, merge-tree). For very large repositories or
slow hooks, raise them:

```sh
export BATTY_GIT_TIMEOUT_SECS=300
export BATTY_GIT_LONG_TIMEOUT_SECS=1800
```

### Background verification

Verification and post-merge tests run in the background. A job that runs longer
than `workflow_policy.verification.timeout_secs` (default 3600) is killed and
counted as a failure. Set it to `0` to disable the limit.

After a failed post-merge verification, batty reverts trunk only if it still
points at that merge; otherwise it escalates for a manual revert. It never
discards uncommitted changes in the project root.

### Telemetry retention

Telemetry rows older than 90 days are deleted, starting on the first daemon tick
after startup. To keep everything, set:

```yaml
telemetry:
  retention_days: 0
```

### Chat integrations

- Commands sent while the daemon was down are still ignored, but batty now
  replies once per chat with the number it skipped (the first poll counts the
  real backlog, shown as `100+` at 100 or more).
- `batty start` no longer refuses an unknown `channel:` or an empty Telegram
  `target`; it prints a warning and starts. `batty validate` still reports both
  as errors. `batty start` never runs the provider `--version` probe; only
  `batty validate` does (5 second timeout, once per process).
- The openclaw provider is called with `--channel telegram --target <id>`. Older
  openclaw releases, or any provider that rejects `--channel`/`--target`
  ("unknown", "unrecognized", "unexpected" or "invalid" naming either flag), are
  handled by an automatic fallback to `--to`. Batty tries the modern form again
  once a day, so an upgraded provider is picked up without a restart.

### Task status moves

The daemon validates status moves against the task workflow. Moves it would
previously have forced (for example `todo` → `done`) are refused and reported to
the manager.

### Removed config keys

These keys still load but have no effect, and `batty validate` warns about them:
`automation.commit_before_reset`, `workflow_policy.narration_threshold`,
`workflow_policy.narration_nudge_max`, `automation.replenishment_threshold`,
`auto_respawn_on_crash`, `capability_overrides` and role `owns`. Remove them when
convenient.

## For library users

The `batty_cli::events`, `batty_cli::log` and `batty_cli::config` modules were
removed. `config::Policy` is now `task::Policy`.
