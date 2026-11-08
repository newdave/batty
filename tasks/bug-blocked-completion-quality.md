---
title: Preserve unfinished tasks across turn completion and reconciliation
status: review
priority: critical
owner: codex/fix-blocked-completion
---

# Incident: TerrainLink JOBS-03 (#204)

## Observed failure

TerrainLink's daemon started completion verification for #204 at
2026-11-08T15:12:10Z. Verification restarted after HEAD changed, failed twice,
and reconciliation cleared the active assignment at 15:17:35Z because the task
had entered `blocked`. Unfinished managed branches can then become eligible for
the idle-worktree reset path. A turn ending is not sufficient evidence that a
task is ready for verification.

Evidence: TerrainLink `.batty/daemon.log`, lines 19060, 19259–19282 and
19341–19345 at investigation time. These offsets are diagnostic pointers, not
stable identifiers. No production resource or cutover change is required.

## Attribution

- The original shim completion routing (`64572b8f`) and Codex SDK support
  (`30c30d5a`) already connected turn completion to engineer completion.
- Upstream reconciliation commit `4c91baf5` introduced resetting idle managed
  branches when the board no longer authorizes them.
- The dispatch branch reset predates fork hardening (`996e14fc`). Our October 27
  archive-error guard (`85b36a3c`) prevents an unsafe reset when saving fails,
  but still resets an existing task branch after a successful archive. A backup
  alone does not resume the original task checkout.
- Our October 27 change `534efd4c` added a stale-completion guard for `done`
  and `archived`, but omitted blocked tasks and board ownership changes.
- The November Jev completion veto (`e9ac41f8`) did not introduce the original
  turn/task conflation. It adds another asynchronous boundary that must respect
  board authority.

The original trunk configuration work is not the source of these behaviors:
the unconditional completion routing and branch reset already existed.

This establishes older contributing behavior and incomplete fork hardening;
it does not prove a single commit alone caused the entire live incident.
The commit IDs above are reachable in this fork; imported duplicate histories
contain other IDs for the same original changes.

## Required regression evidence

- A blocked or reassigned task cannot start tests, consume retries, change board
  ownership, or enter merge processing through a stale completion.
- A blocker-only reply with the board still in progress cannot be accepted as
  completed work. Test the actual shim event path, not only a helper.
- A board status or owner change while tests or the completion veto run must
  invalidate the result, even if the in-memory active-task map has not caught up.
- A queued continuation cannot race completion verification against the same
  engineer's live edits.
- Idle cleanup preserves unfinished blocked branches and committed work without
  requiring an active claim. Reassignment must not overwrite saved task refs.
- Legitimate completed work still reaches review; done/archived cleanup and
  genuine abandoned-branch recovery retain their expected behavior.

## Delivery gate

Show failing regressions before the fix and passing results after it. Run the
relevant completion, reconciliation, dispatch and worktree tests, then the full
default suite, formatting and clippy. Use a private tmux server for any
integration coverage. Review the final diff independently before publishing.

Treat local test success, merged code, installed binary and recovered live
TerrainLink progress as separate evidence. Do not label the live incident fixed
until the running daemon uses the patched build and a safe resumed task retains
its ownership and branch through the relevant transitions.

## Independent review findings

- Queue suppression must not discard an architect's completed planning response
  merely because another message is queued. Exercise the architect event path
  and preserve its working state for the queued turn.
- Packet parsing must find an explicit completion packet after an earlier shell
  or test-output fence. Requiring a packet must not strand valid older project
  prompts without a bounded, actionable correction message.
- Keep the terminal-task cleanup path reachable before requiring a ready packet.
- A follow-up turn arriving after verification starts must invalidate pending
  test and veto results, even with unchanged HEAD. Independent final review
  reproduced both failures: stale evidence was published, and a parked veto
  result merged work while the engineer's next turn was active. The shared
  completion eligibility check now requires an idle shim and no in-flight
  messages; both regressions pass after the correction.

These findings concern the proposed repair, not attribution of the original
incident. The architect, packet-selection and bounded-reminder regressions each
failed before correction and passed afterward. Final acceptance still requires
the complete delivery gate. The asynchronous veto ownership regression also
passes for both a board-only block and reassignment, preserving the merge target.


## Final local gate

The final repair passed the complete default Rust suite (4,370 library tests,
zero failures), formatting, all-target/all-feature Clippy, version-policy unit
tests and the targeted private-tmux lifecycle regression. Independent final
review approved the corrected follow-up-turn guard. Hosted CI, installation and
live recovery remain explicitly unverified by this local gate.
