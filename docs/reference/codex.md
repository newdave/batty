# Codex in Batty

Use Codex for bounded repository work: implementation, tests, debugging and an
independent QA pass. Keep architecture, coordination and sourced research on
Claude initially; keep documentation on Haiku. This allocation moves execution
turns away from Claude. It is an operational recommendation, not a benchmarked
quality or price comparison. Both providers retain their own usage limits.

## Recommended role defaults

The opt-in `mixed` template provides:

| Role                     | Backend | Model selection              | Effort      |
| ------------------------ | ------- | ---------------------------- | ----------- |
| General engineers        | Codex   | Existing Codex model setting | medium      |
| Quality specialist       | Codex   | Existing Codex model setting | medium      |
| Architect and manager    | Claude  | Sonnet alias                 | medium      |
| Research specialist      | Claude  | Sonnet alias                 | medium      |
| Documentation specialist | Claude  | Haiku alias                  | No override |

Use `batty init --template mixed` in a new workspace. Init keeps each role's
backend; passing `--agent` instead sets one backend for every role. The `simple` template keeps
its existing Claude allocation. For an existing team, edit the selected roles
rather than force-initialising the repository or replacing its prompts:

```yaml
- name: engineer
  role_type: engineer
  agent: codex
  effort: medium
  instances: 2
  use_worktrees: true
```

Remove the previous provider's model alias when switching backends. Omit `model`
to retain Codex's selected model, or configure an explicit model available to the
account. A global `init --agent` override clears model and effort pins when it
changes a role's backend, including inherited instance settings. Configure any
required overrides after choosing the backend.

Keep QA in a separate member and thread, with independent acceptance evidence.
Use Claude for QA when a review from another backend is desired. Model effort
escalation should be bounded and tied to a demonstrated reasoning gap.

## Session and execution behaviour

Batty's Codex provider prompt requests the installed Superpowers skills, starting
with `superpowers:using-superpowers`. Agents load relevant skills on demand for
debugging, tests, and verification; Batty does not copy entire skill bodies into
every assignment. Both `codex` and `codex-cli` use this guidance through the default
Codex provider overlay, for interactive launch context and SDK role context.
An explicitly configured provider overlay takes precedence.

The same overlay requests installed Caveman skills: `caveman` for terse replies,
`caveman-explore` for broad repository discovery, `caveman-review` for review
comments, and `caveman-commit` when drafting commit messages. Agents preserve
technical payloads and completion evidence, use normal prose in persisted
artifacts, and respect explicit style overrides. File compression and sending
repository content to another provider require explicit authorisation.

Superpowers and Caveman must already be available to the Codex process. If unavailable, the
agent is instructed to report that once and continue with repository rules and
verification. Batty does not install plugins or modify global configuration.
Batty retains ownership of dispatch, worktrees, and merges; Superpowers does not
replace the queue or establish that queue-related change loss is fixed.

The guidance applies on the next launch with newly rendered role context.
Fresh SDK sessions preserve both role instructions and any restart handoff.
Existing SDK threads retain their earlier instructions until a fresh session;
resumed SDK turns do not resend the role context.

Codex SDK mode uses `codex exec --json` and resumes only an explicitly recorded
provider thread ID. A new member has no thread ID until the provider reports
`thread.started`; Batty does not invent an ID or select `--last`. The first turn
includes the composed role instructions and assignment context. Resumed turns do
not repeat that context unnecessarily. Runtime restarts retain the recorded
thread, while a deliberate cold start clears it.

The daemon persists the reported identity for the correct member and rejects
shared thread identities. Session tracking and cost attribution use the exact
provider ID, including when the SDK working directory differs from an interactive
launch directory. An unbound SDK member does not inherit an older transcript.
Member-scoped board wrappers are available to SDK commands.

Live usage follows completed-turn snapshots. Duplicate snapshots are merged;
cached input is an inclusive subset of input, so it is not added twice. Context
capacity stays unknown when the provider does not report it. Pricing remains
unknown unless an applicable rate is configured; API estimates are not
subscription billing.

A process that fails to start, exits unsuccessfully or never completes its turn
does not produce an assignment-completion event. Queued deliveries are surfaced
as failures. Shutdown stops the active owned execution before preserving the
worktree and prevents queued execution from restarting it. Terminal authentication,
quota and model-configuration errors retain the existing parking behaviour.

## Validation and limits

Static preflight and normal validation do not start Codex or perform inference.
An explicit `backend-probe` makes one live request; its Codex command is ephemeral,
read-only and non-interactive. Probe transcripts do not enter normal session
accounting. Authentication remains in the user's existing Codex configuration;
Batty does not copy credentials or create alternate authentication homes.

The mixed template retains Batty's existing Codex execution permission policy,
including its approval/sandbox bypass flag. Use trusted, isolated worktrees and
review project rules and hooks before unattended execution. Selecting the mixed
template does not itself validate account access or model availability.

Compatibility was checked against installed Codex CLI 0.164.0 help and the
[official non-interactive documentation](https://developers.openai.com/codex/noninteractive).
Regression tests use fake subprocesses; live account/model compatibility and
comparative token savings were not measured.
