# Agent efficiency and session accounting

Batty resolves optional `model` and `effort` settings separately for every role
and named instance. Omitting them preserves the backend's own defaults. Launch
metadata in `.batty/launch-options/` records requested settings, never credentials;
`batty preflight --runtime` compares those settings and session identities with
the current configuration. This is a consistency check, not proof of the model
actually served by a provider.

```yaml
roles:
  - name: reviewer
    role_type: engineer
    agent: claude
    model: your-selected-model
    effort: low
    on_demand: true
workflow_policy:
  idle_inference_enabled: false
  zero_diff_recovery: park
  startup_validation_timeout_secs: 30
board:
  tag_roles:
    review: reviewer
```

`effort` is supported for Claude and Codex. Named `instance_overrides` can change
it independently. `on_demand` excludes a role from general automatic assignment;
a matching tag route, explicit assignee, or task body role can select it. It does
not defer creation of that member's process. Conflicting tag routes hold the task
for clarification. Layout zones accept an ordered `members` list containing
member names or role names, so specialists can share zones without pane scripts.
`docs_only_evidence: true` lets a role pass the completion evidence check with
committed documentation-only changes (`docs/`, Markdown and other non-code
paths). Without it, a completion needs a changed code file, so work from a
documentation role would be rejected as narration-only.

The default simple team contains an architect, a manager, two general engineers,
and research, documentation and quality specialists. Its standup interval is
30 minutes with 12 output lines; the architect nudge interval is one hour. Idle
inference and architect utilisation interventions are disabled, stall detection
waits 15 minutes, restart attempts are capped at one, and automatic crash respawn
is disabled. Verification remains enabled with two iterations and evidence.
Routine Claude roles select the Sonnet alias with medium effort; the documentation
specialist selects Haiku without an effort override. These are template choices,
not forced defaults for other configurations. Existing explicit configuration values take precedence. These defaults also
change omitted settings in existing teams; review `batty config` before starting.

Transport Ping/Pong remains active when idle inference is disabled. Claude SDK
context pressure uses per-request input, cache and output usage and explicit provider context
window metadata. Cumulative session totals do not determine request pressure.
Unknown context capacity stays unknown. Each member's pinned session is kept
separate; a newer transcript belonging to another member does not replace it.

Terminal backend configuration errors persist a generic marker under
`.batty/backend-configuration/`. Repair the backend settings, then run
`batty backend-reset MEMBER`; a running daemon recovers only that member at its
next health check. Zero-diff `park` recovery uses the existing attribution guard
and benches the affected engineer rather than repeatedly restarting it.

`batty preflight` performs static checks without inference. Startup takes a local
exclusive lock and can perform bounded runtime consistency checks, rolling back
the resources created by that startup if readiness fails. A leftover
`.batty/startup.lock` requires inspection after a killed startup. Setting
`startup_validation_timeout_secs: null` disables the post-start check.

`batty backend-probe MEMBER --timeout-secs 30` is an explicit, bounded live model
request. It uses that member's selected backend, model, effort and authentication,
and records limited response metadata under `.batty/probes/`. It may consume
subscription usage or API tokens. Preflight and validation never run it.

## Cost and accepted outcomes

`batty cost --json` emits machine-readable token counts, warnings and pricing
coverage. Exact session paths and stable request/message identities are deduped.
Cache and reasoning counters are inclusive subsets where providers define them
that way. A recorded session ID never falls back to an unrelated latest session.
Mixed-model sessions without per-model attribution have unknown pricing.
Incomplete pricing produces a null complete total plus a known subtotal; API
price estimates do not represent subscription billing.

Use `batty outcome-record TASK --evidence FILE` to record explicit acceptance of
code or non-code work. Optional `--commit SHA` must already be integrated into the
configured trunk; `--session ID` links a session and `--defects N` records defects.
The evidence hash makes repeated records idempotent. This does not merge code or
change board status. `batty outcome-report --since SECONDS --until SECONDS` uses
inclusive Unix timestamp bounds and counts the latest accepted record per task.
Tokens per accepted task remain unknown until exact usage-window attribution is
available; the report does not manufacture an efficiency ratio.

## Board compatibility

Native task mutations preserve structured metadata, reject stale snapshots and
validate claim changes, and restore the exact original file mode after writing. On Unix
systems Batty serialises its mutations with the board's `.lock` flock, matching
kanban-md 0.39.0's create/TUI protocol. That backend's standalone edit/move CLI
does not take this lock; external callers must coordinate raw CLI mutations.
Native mutation on other platforms currently returns an explicit unsupported
error. Batty does not remove or replace the shared lock inode.

## Verification commands

Verification uses an explicit `workflow_policy.verification.test_command` first,
then the legacy policy command. Otherwise it detects `make check`, `make test`
or `cargo test` from project files, in that order. Other projects must configure
a command; preflight and startup report this before launching a team. Non-code
deliverables can explicitly disable automatic tests.

For an opt-in Codex allocation and SDK behaviour, see [Codex in Batty](codex.md).
