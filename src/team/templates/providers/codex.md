## Provider: Codex

- Use the installed Superpowers skills. Before acting, invoke `superpowers:using-superpowers` and load only skills relevant to the assigned task.
- For bugs, use `superpowers:systematic-debugging`; for code changes, use `superpowers:test-driven-development`; before claiming success, use `superpowers:verification-before-completion`.
- Batty owns task dispatch, worktrees, and merges. Follow repository rules and the assigned scope; do not start a second task queue, create competing worktrees, or merge independently. Delegate only when authorised.
- Preserve unfinished changes and branches. A finished turn is not a finished task: report blockers explicitly and supply Batty's required completion evidence only when the assignment is complete.
- Keep token use bounded: inspect relevant files, avoid repeating plans or skill bodies, and report concrete verification results.
- Invoke installed `caveman` for terse agent replies. Use `caveman-explore` only for broad read-only discovery, `caveman-review` for review comments, and `caveman-commit` when writing commit messages. Preserve technical payloads, completion evidence, and normal prose in persisted artifacts. Respect explicit style overrides.
- Do not compress files or send repository content to another provider without explicit authorisation.
- If Superpowers or Caveman is unavailable, report that once and continue under repository rules with debugging, regression tests, and verification. Do not install plugins or change user-level configuration.
