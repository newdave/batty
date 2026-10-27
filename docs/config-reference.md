# Team Config Reference

This page documents the human-edited `team.yaml` surface under
`.batty/team_config/`.

## Complete Example

```yaml
name: my-project
agent: claude
trunk_branch: main
workflow_mode: hybrid
use_shim: true
use_sdk_mode: true
orchestrator_pane: true
orchestrator_position: left
automation_sender: human
external_senders: [slack-bridge]
shim_health_check_interval_secs: 30
shim_health_timeout_secs: 90
shim_shutdown_timeout_secs: 10
shim_working_state_timeout_secs: 1800
pending_queue_max_age_secs: 600
event_log_max_bytes: 5242880
retro_min_duration_secs: 900

board:
  rotation_threshold: 20
  auto_dispatch: true
  worktree_stale_rebase_threshold: 5
  auto_replenish: true
  state_reconciliation_interval_secs: 30
  dispatch_stabilization_delay_secs: 30
  dispatch_dedup_window_secs: 60
  dispatch_manual_cooldown_secs: 30

standup:
  interval_secs: 300
  output_lines: 40

automation:
  timeout_nudges: true
  standups: true
  clean_room_mode: false
  failure_pattern_detection: true
  triage_interventions: true
  review_interventions: true
  owned_task_interventions: true
  manager_dispatch_interventions: true
  architect_utilization_interventions: true
  intervention_idle_grace_secs: 60
  intervention_cooldown_secs: 300
  utilization_recovery_interval_secs: 900

workflow_policy:
  clean_room_mode: false
  handoff_directory: .batty/handoff
  wip_limit_per_engineer: 1
  wip_limit_per_reviewer: 2
  pipeline_starvation_threshold: 1
  escalation_threshold_secs: 1800
  review_nudge_threshold_secs: 1800
  review_timeout_secs: 7200
  stale_in_progress_hours: 4
  aged_todo_hours: 24
  stale_review_hours: 4
  stall_threshold_secs: 120
  max_stall_restarts: 5
  health_check_interval_secs: 30
  planning_cycle_cooldown_secs: 300
  narration_detection_enabled: true
  narration_threshold_polls: 5
  context_pressure_threshold: 100
  context_pressure_threshold_bytes: 512000
  context_pressure_restart_delay_secs: 120
  graceful_shutdown_timeout_secs: 5
  auto_commit_on_restart: true
  uncommitted_warn_threshold: 20
  context_handoff_enabled: true
  handoff_screen_history: 20
  verification:
    max_iterations: 5
    auto_run_tests: true
    require_evidence: true
    test_command: cargo test
    timeout_secs: 3600   # background verification/post-merge test limit; 0 disables
  claim_ttl:
    default_secs: 1800
    critical_secs: 900
    max_extensions: 2
    progress_check_interval_secs: 120
    warning_secs: 300
  allocation:
    strategy: scored
    tag_weight: 15
    file_overlap_weight: 10
    load_penalty: 8
    conflict_penalty: 12
    experience_bonus: 3
  auto_merge:
    enabled: true
    max_diff_lines: 200
    max_files_changed: 5
    max_modules_touched: 2
    sensitive_paths: [Cargo.toml, team.yaml, .env]
    confidence_threshold: 0.8
    require_tests_pass: true
    post_merge_verify: true
    lock_busy_escalate_after_secs: 600  # escalate once the merge lock stays busy this long, then hourly

grafana:
  enabled: true
  port: 3000

# Telemetry database (.batty/telemetry.db)
telemetry:
  retention_days: 90   # prune events older than this; 0 disables pruning.
                       # First pass runs on daemon startup, then every 24h.

roles:
  - name: human
    role_type: user
    channel: telegram
    channel_config:
      provider: openclaw
      target: "123456789"
      allowed_user_ids: [123456789]
    talks_to: [architect]

  - name: architect
    role_type: architect
    agent: claude
    prompt: batty_architect.md
    posture: orchestrator
    model_class: frontier
    talks_to: [human, manager]

  - name: manager
    role_type: manager
    agent: claude
    prompt: batty_manager.md
    posture: orchestrator
    model_class: frontier
    talks_to: [architect, engineer]

  - name: engineer
    role_type: engineer
    agent: codex
    instances: 3
    prompt: batty_engineer.md
    posture: deep_worker
    model_class: standard
    nudge_interval_secs: 900
    use_worktrees: true
    talks_to: [manager]
```

## Top-Level Fields

| Key                                               | Purpose                                                    |
| ------------------------------------------------- | ---------------------------------------------------------- |
| `name`                                            | Team/session name                                          |
| `agent`                                           | Team-wide default backend when a role does not override it |
| `trunk_branch`                                    | Branch that worktrees start from and merges land on (default `main`) |
| `workflow_mode`                                   | `legacy`, `hybrid`, or `workflow_first`                    |
| `use_shim`                                        | Run members through the managed shim runtime               |
| `use_sdk_mode`                                    | Prefer structured protocols over PTY parsing               |
| `orchestrator_pane` / `orchestrator_position`     | Show the orchestration surface in tmux                     |
| `external_senders`                                | Allow non-team sources to message roles                    |
| `shim_*` and `pending_queue_max_age_secs`         | Runtime health and delivery tuning                         |
| `event_log_max_bytes` / `retro_min_duration_secs` | Log and retrospective limits                               |

### Merging To An Integration Branch

Repos that protect `main` and require pull requests can point Batty at an
integration branch instead:

```yaml
trunk_branch: develop
```

Engineer worktrees then branch from `develop`, and both the merge queue and
`batty merge` land completed work on `develop`. Batty does not push or open
PRs; open a PR from `develop` into `main` on your own schedule. The branch
must exist locally or as `origin/develop`; when only the remote branch exists,
`batty start` creates a local tracking branch.

## `layout`

```yaml
layout:
  mode: panes       # panes | auto | windows | focus (default: panes)
  keys: false       # F-key bindings in a batty-only key table (default: false)
  zones: []         # optional hand-built tiled layout (panes mode)
```

- `auto`: tiled panes when at most 4 agent panes fit at 50x12 or larger each;
  otherwise `windows`. If `zones` is set and `mode` is omitted, panes are kept.
- `panes`: every member tiled in one window.
- `windows`: window 1 is a live status dashboard, then one window per member
  in hierarchy order, with a clickable tab bar and F-key jumps.
- `focus`: one big pane plus a narrow column; F2-F10 swap a member in, F11 cycles.

- `keys`: when `true`, Batty installs F1 (help), F2-F10 (jump), F12 (zoom) and
  F11 (focus) in a key table used only by its own session. Default `false`:
  that key table currently disables tmux's mouse and root key bindings in the
  session (clicking panes, wheel scrolling, clickable tabs).

See [tmux-layouts.md](tmux-layouts.md) for the comparison and key reference.

## `board`

`board` controls the daemon's view of runnable work.

- `auto_dispatch`: assign `todo` work to idle engineers automatically
- `auto_replenish`: create planning pressure when backlog runs dry
- `worktree_stale_rebase_threshold`: how many stale-base checks occur before rebase/reset
- `state_reconciliation_interval_secs`: resync daemon state with board ownership
- `dispatch_*`: dedup, cooldown, and stabilization timings

## `automation`

These toggles turn runtime recovery systems on or off.

- `timeout_nudges` and `standups` are timer-based safety nets
- `triage_interventions`, `review_interventions`, and `owned_task_interventions`
  handle common workflow stalls
- `manager_dispatch_interventions` and `architect_utilization_interventions`
  keep the hierarchy moving when reports go idle
- `failure_pattern_detection` feeds long-running reliability analysis
- `replenishment_threshold` has no effect; remove `automation.replenishment_threshold`
  (see the migration guide)

## `telemetry`

- `retention_days` (default 90): telemetry rows older than this are deleted,
  first on daemon startup and then every 24 hours. `0` keeps everything.

## `workflow_policy`

This block controls execution quality and merge safety.

- `verification`: completion retry limits and test command policy.
  `verification.timeout_secs` (default 3600) kills a background verification or
  post-merge test job that runs longer and counts it as a failure; `0` disables
  the limit
- `claim_ttl`: stale-ownership reclaim timings
- `allocation`: scored assignment weights
- `main_smoke`: periodic `main` smoke test and dispatch-gate policy
- `auto_merge`: unattended merge thresholds and post-merge verification
- `context_*` and `handoff_*`: context-pressure restart and handoff behavior
- `review_*` and `stale_*`: escalation thresholds for aging work
- `narration_*`: guard rails against agents narrating instead of changing code

`workflow_policy.main_smoke` fields:

- `enabled`: turn periodic `main` smoke checks on or off. Default: `true`
- `interval_secs`: smoke-check cadence. Default: `600`
- `command`: shell command run at the project root. Default: `cargo check` when the
  project has a `Cargo.toml`, otherwise the verification command (an explicit
  `verification.test_command`, or a detected `make check` or `make test`). With none,
  the smoke check is skipped rather than reporting `main` broken.
- `pause_dispatch_on_failure`: stop auto-dispatch while `main` is broken. Default: `true`
- `auto_revert`: optionally revert `HEAD` after a failing smoke run. Default: `false`

## `roles`

Each role entry defines topology and behavior for one role class.

Common fields:

- `name`, `role_type`, `agent`, `instances`
- `prompt`, `talks_to`, `use_worktrees`
- `posture` and `model_class` for template- and model-selection hints
- `channel` and `channel_config` for user-facing endpoints such as Telegram
- `nudge_interval_secs`, `receives_standup`, `standup_interval_secs`
- `provider_overlay` and `instance_overrides` for per-member specialization
- `auth_mode` / `auth_env` when a backend needs explicit auth posture

### openclaw provider compatibility

With `provider: openclaw` and no bot token, Batty runs
`openclaw message send --channel telegram --target <id> --message <text>`.
If the provider rejects `--channel` or `--target` (stderr names the flag together
with "unknown", "unrecognized", "unexpected" or "invalid"), Batty retries once
with the legacy `--to <id>` form and uses it for later sends. It tries the modern
form again after 24 hours.
The `--target` form was verified against openclaw 2026.5.26; the exact first
release that introduced it is not documented. `batty validate` warns (never errors)
when `<provider> --version` reports something older than 2026.5.26. The version
probe runs only in `batty validate`, with a 5 second timeout, once per process.

### Channel validation

`batty validate` reports an unknown `channel:` or an empty Telegram `target` as
an error. `batty start` only prints a warning for the same problems and starts
anyway (the integration cannot deliver until you fix it), and never runs the
provider version probe.

## Recommended Defaults For Unattended Teams

- Keep `use_shim: true` and `use_sdk_mode: true`.
- Leave `board.auto_dispatch: true` and `workflow_policy.auto_merge.enabled: true`.
- Enable `use_worktrees: true` for engineers.
- Use `workflow_mode: hybrid` unless you are intentionally forcing workflow-first behavior.

## Git Environment Variables

Every `git` command Batty runs has a timeout. Commands are split into two classes:

- **Long** (default 600s): network and object-store commands (`fetch`, `push`, `pull`, `clone`, `gc`, ...), commands that run hooks (`commit`, `merge`, `rebase`, ...), and commands that walk or rewrite the index or working tree (`checkout`, `switch`, `worktree`, `reset`, `stash`, `add`, `status`, `clean`, `diff`, `merge-tree`).
- **Short** (default 120s): everything else, mainly read-only plumbing such as `rev-parse`, `rev-list`, `for-each-ref` and `config`.

| Variable | Default | Effect |
|----------|---------|--------|
| `BATTY_GIT_TIMEOUT_SECS` | `120` | Timeout in seconds for short-class git commands. |
| `BATTY_GIT_LONG_TIMEOUT_SECS` | `600` | Timeout in seconds for long-class git commands. |

Values must be positive integers; anything else falls back to the default.

On timeout Batty sends SIGTERM to the command's process group, waits 5 seconds, then sends SIGKILL. If the killed command left a stale `.git/index.lock` behind (modified after the command started, with no process left in its group), Batty removes it and logs a warning. A timeout is reported as a permanent error and is not retried.

When Batty runs from a terminal, `commit`, `merge`, `rebase`, `tag` and `push` stay in the terminal's foreground process group so hooks and signing programs can read `/dev/tty` and Ctrl-C reaches git. The daemon has no terminal and always isolates git in its own process group.
