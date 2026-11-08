---
title: Synchronize Git timeout test fixtures before asserting lock preservation
status: review
priority: high
---

The final blocked-completion validation run reported failures in
`git::tests::timeout_keeps_index_lock_written_by_a_concurrent_process` and
`git::tests::timed_out_fetch_never_touches_index_lock`. Both passed in focused
runs. This is evidence of inconsistent results, not proof that the failures are
harmless or that the final suite passed.

The concurrent writer uses a fixed 0.2-second sleep before creating the lock;
the fetch fixture assumes its remote helper creates the lock within an
800-millisecond command timeout. Verify actual panic diagnostics and replace
these scheduling assumptions with explicit fixture readiness where appropriate.
Do not weaken the production rule that another process's lock must survive.

Acceptance:

- Demonstrate the fixture race or identify a different verified cause.
- Prove that timeout cleanup preserves a live concurrent writer's index lock.
- Prove that timing out fetch never removes an existing index lock.
- Run the regressions under bounded concurrent load and as part of the complete
  suite; retain all failures rather than selecting only passing reruns.
- Keep this test-hardening work separate from blocked-task behavior changes.

Evidence logs at investigation time:
`/private/tmp/batty-final-tests.log` and
`/private/tmp/batty-lock-diagnostic.log`. These temporary logs are diagnostic
pointers; capture decisive failures in durable review evidence before cleanup.

## Confirmed cause and repair

The complete run failed before any cleanup claim could be established: the
concurrent writer created `index.lock` before Git acquired its initial lock, so
Git exited with `File exists` instead of timing out. The fetch helper had not
created its lock before timeout. The writer now waits for a pre-commit readiness
marker; the fetch lock exists before command startup. An explicit classification
assertion also verifies that fetch never enters index-writing cleanup. The
focused Git suite passed all 22 tests after these fixture changes.

The final complete Rust suite also passed after these fixture corrections.
