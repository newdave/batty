# Preserve blocked task ownership and continuation

Status: ready for review

A transport completion event ends an agent turn, including a blocker reply. Native
completion handling must check current board status and ownership before counting
zero-diff completions or launching verification. Results finishing after a task is
blocked or reassigned must not consume retries or route stale work. Idle cleanup
must preserve blocked task checkouts, and redispatch must resume saved commits.

Acceptance:

- Blocked and reassigned tasks cannot start completion verification.
- Blocked task branches survive claim release and idle reconciliation.
- Redispatch of an existing task branch retains its committed work.
- Follow-up turns invalidate earlier asynchronous test and veto results without
  changing ownership, consuming retries or merging the still-active checkout.
- Engineer instructions record blockers on the board before ending a turn.
- Focused regressions, the full test suite, formatting and Clippy pass.
- Production resources and cutover remain outside this repair.

## Verification evidence

Focused regressions cover blocked and reassigned owners, queued engineer turns,
queued architect responses, packet selection, bounded protocol reminders, stale
asynchronous verification and veto results, idle preservation and redispatch.
The private-tmux lifecycle regression passed, including verification and merging
a legitimate completed task. Its fixture now establishes the board claim that
real dispatch owns; the authority guard remains unchanged.

Independent review approved the final repair source and required regression
coverage. Full-suite, CI, installed binary and live recovery remain separate
acceptance gates until their results are recorded.

## Final local validation

The final 0.12.2 repair tree passed `cargo test`: 4,370 library tests,
154 binary tests and nine integration contract tests, with no failures. The
154 tmux-dependent library tests remain ignored in the default suite.
`make lint` passed formatting and all-target/all-feature Clippy with warnings
denied. The targeted private-tmux daemon lifecycle test passed on the final tree.
All 12 version-policy tests passed, and the CI workflow parsed as valid YAML.

A final independent review reproduced an additional follow-up-turn race, then
approved the corrected shared guard after both new regressions passed.
No installed-binary, hosted-CI or live TerrainLink recovery claim follows from
these local checks. Those remain separate delivery evidence.
