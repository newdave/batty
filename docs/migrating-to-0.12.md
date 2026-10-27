# Migrating to 0.12

This guide covers changes you may need to act on when upgrading from batty 0.11.x
(including upstream `battysh/batty`). The full list is in the
[changelog](../CHANGELOG.md).

## Upgrade steps

1. Stop running teams with `batty stop` before installing the new binary. The
   merge lock is compatible with older binaries, but stopping first avoids a
   daemon hot-reloading in the middle of a merge.
2. Install the new binary and run `batty validate` in each project. Fix any
   errors it reports about channels; review its warnings about unused keys.
3. Start the team with `batty start`. If the team has more than four agents,
   the tmux session now opens with one window per member (see
   [tmux layouts](tmux-layouts.md)).

## Behaviour changes to check

### tmux layout

`layout.mode` defaults to `auto`. Teams with more than four agent panes get a
dashboard window plus one window per member instead of tiled panes. Window
numbers start at 1. To keep the old layout:

```yaml
layout:
  mode: panes
```

Configs that already define `layout.zones` keep tiled panes. Batty's function
keys (F1–F12) apply only inside batty sessions; set `layout.keys: false` to turn
them off.

### `batty status`

The default view is a summary plus a compact table. Use `batty status --wide`
for every column. Scripts should use `batty status --json`, which is unchanged.

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
  replies once per chat to say how many were skipped.
- The openclaw provider is called with `--channel telegram --target <id>`. Older
  openclaw releases that only accept `--to` are handled by an automatic fallback.

### Task status moves

The daemon validates status moves against the task workflow. Moves it would
previously have forced (for example `todo` → `done`) are refused and reported to
the manager.

### Removed config keys

These keys still load but have no effect, and `batty validate` warns about them:
`automation.commit_before_reset`, `workflow_policy.narration_threshold`,
`workflow_policy.narration_nudge_max`, `replenishment_threshold`,
`auto_respawn_on_crash`, `capability_overrides` and role `owns`. Remove them when
convenient.

## For library users

The `batty_cli::events`, `batty_cli::log` and `batty_cli::config` modules were
removed. `config::Policy` is now `task::Policy`.
