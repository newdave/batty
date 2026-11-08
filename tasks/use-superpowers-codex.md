---
title: Use Superpowers in Codex role context and preserve it across restart handoffs
status: done
priority: high
---

Codex agents should use installed Superpowers skills for debugging, regression
tests, and verification, with compact guidance and Batty retaining ownership of
dispatch, worktrees, and merges. Missing skills must be reported without silently
installing plugins or changing global configuration.

Independent review found that a fresh SDK launch after a restart handoff drops
the composed role instructions: `prepare_member_launch` replaces the launch
prompt with handoff text, and `write_launch_script` saves only that prompt as SDK
role context. The nested interactive `AGENTS.md` does not supply instructions to
an SDK process launched from the worktree root.

History attributes the handoff replacement to upstream commit `888e12b5` and
the SDK context serialization to fork commit `45daa85f`. Their interaction drops
role instructions; this does not establish the cause of queue-related code loss.
That incident remains tracked in `bug-blocked-completion-quality.md`.

Acceptance:

- Default Codex and Codex CLI provider overlays request relevant installed skills.
- Other providers retain their existing instructions.
- Interactive launch context and fresh SDK context include Superpowers guidance.
- Distinct SDK handoff and role instructions both survive serialization.
- Identical role and launch prompts are not duplicated; resumed turns do not
  repeat the role context.
- Focused regressions, `cargo test`, `make lint`, and independent review pass.
- Report local validation separately from installed binary or live recovery.

Validation evidence:

- The provider-prompt regression failed before adding Superpowers guidance.
- The SDK handoff regression then failed before preserving distinct role context.
- All three focused Superpowers regressions pass after both corrections.
- `make lint` passes; independent final review returned APPROVE.
- The subsequent combined Superpowers/Caveman feature snapshot passes the full
  default suite in an isolated checkout: 4,523 tests passed, 154 ignored, and no
  failures. See `use-caveman-codex.md` for the environment controls and scope.
- No installed binary, live-provider execution, or recovery of the separate
  queue incident is claimed.
