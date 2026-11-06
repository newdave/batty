# Brainstorm: bench recovery automation

## 1. Session overview

- **Date:** 2026-11-07
- **Feature:** Automation that keeps engineers from being benched and gets them unbenched; automatic task approval when every engineer is benched.
- **Participants:** project owner (decisions), facilitator.
- **Outcome:** decided scope, safety bar and defaults; prioritised backlog below.

## 2. Problem and context

Benching removes an engineer from dispatch. The daemon skips benched engineers and prunes their queue entries (`src/team/daemon/dispatch/queue.rs`). Bench state is a file managed by `src/team/bench.rs`.

There are two ways an engineer gets benched today:

- **Manual:** `batty bench <engineer>` (and `batty unbench`).
- **Automatic:** with `workflow_policy.zero_diff_recovery: park` (the default), an engineer is parked after `ZERO_DIFF_COMPLETION_RECOVERY_THRESHOLD` (2) completion reports with no attributed commit on the task branch. The bench reason is "completion attribution requires operator review" (`src/team/daemon/health/poll_shim.rs`).

Nothing unbenches automatically. Once every engineer is parked, the run stops making progress until a human reviews, unbenches and re-dispatches. Tasks waiting in `review` also wait for a manager who has no one to hand rework to. This conflicts with the project's throughput priority: tasks should flow from todo to done without human intervention.

## 3. Decisions

| Topic                                                 | Decision                                                                                                                                                                                                                                                               |
| ----------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| What becomes automatic when all engineers are benched | Both the operator review needed to unbench a parked engineer, and approval of tasks in `review`.                                                                                                                                                                       |
| Safety bar for auto-approval and auto-unbench         | **Moderate:** the task branch has commits ahead of trunk and the verification tests pass. Scope-fence violations do not block.                                                                                                                                         |
| Parked task that fails the bar                        | Reset it to `todo` with a note, unbench the engineer, and dispatch it to a **different** engineer.                                                                                                                                                                     |
| Reset cap                                             | 2 automatic resets per task. After that the task goes to `blocked` and is escalated to the operator.                                                                                                                                                                   |
| Effort on retry                                       | **Task-scoped:** the engineer who takes a reset task is relaunched one effort level higher (for example medium to high) for that task only, and drops back to their configured effort afterwards. Each further reset adds one more level, up to the backend's maximum. |
| No other engineer available                           | Give the task back to the same engineer at the raised effort.                                                                                                                                                                                                          |
| Prevention before parking                             | Wrong-branch rescue, an explicit warning one strike before parking, and a timed unbench (below). Auto-committing uncommitted work is **out of scope**: committing stays the agent's job.                                                                               |
| Timed unbench                                         | Only for benches the daemon applied itself (zero-diff park). Manual `batty bench` entries are never auto-unbenched. The cool-off is 30 minutes. Applies even when only some engineers are benched.                                                                     |
| Rollout                                               | **On by default.**                                                                                                                                                                                                                                                     |

## 4. Solution outline

### 4.1 Bench provenance

Record who applied each bench: `BenchEntry.source = manual | auto_park`, plus `benched_at`, `task_id` and a reset counter per task. Store it in the existing bench file and stay compatible with existing entries, which default to `manual`. Every automatic action below acts only on `auto_park` entries.

### 4.2 Prevention (before a strike becomes a park)

1. **Wrong-branch rescue.** On a completion with no attributed commit, look for the engineer's commits since the task was assigned on other branches in their worktree: the base branch `eng-main/<engineer>` or a stray branch. If found, cherry-pick them onto the task branch, then re-run the attribution check before counting a strike. Record a `completion_branch_rescued` event. If the cherry-pick conflicts, abort and count the strike as usual.
1. **Strike warning.** When a strike leaves the engineer one short of the threshold (strike 1 of 2 today), send a direct message: "Task #N: your completion had no commit on `<task branch>`. Commit your work there, then report done again. One more such completion parks you." Record `completion_attribution_warning`.

### 4.3 Timed unbench (some engineers benched)

Every poll, unbench each `auto_park` entry older than `bench_recovery.cooloff_secs` (1800). Its parked task goes through the reset path in 4.4.

### 4.4 Reset path (per parked task)

1. If the task branch meets the moderate bar, send it on through the normal completion path, then unbench.
1. Otherwise, if the task has fewer than `bench_recovery.max_auto_resets` (2) resets:
   - move it to `todo` with a reset note,
   - unbench the engineer,
   - mark the task "avoid `<engineer>`" and raise its effort level by one,
   - let dispatch pick another engineer, or the same engineer if no one else is free.
1. Otherwise, move the task to `blocked`, escalate to the operator on the configured channel, and unbench the engineer so they can take other work.

### 4.5 All engineers benched

When every engineer member is benched:

- Run 4.4 immediately for every `auto_park` entry, without waiting for the cool-off.
- Auto-approve every `review` task that meets the moderate bar. Route it into the existing merge queue as if the manager had approved it, with `approved_by: daemon (all engineers benched)` recorded in the task, an event and the orchestrator log.
- `review` tasks that fail the bar go through 4.4.
- If all benches are manual, do nothing automatic. Alert the operator once per hour that the team is fully benched.

### 4.6 Task-scoped effort

Add an optional `effort_override` to the task. Dispatch passes it to the launch options of the engineer who takes the task, relaunching that member if its current effort differs. Completion clears it, and the member is relaunched at its configured effort on its next assignment. The effort ladder comes from the backend: Claude low/medium/high/xhigh/max, Codex low/medium/high/xhigh. At the top of the ladder the level stays at the top.

### 4.7 Configuration

```yaml
workflow_policy:
  bench_recovery:
    enabled: true            # on by default
    cooloff_secs: 1800
    max_auto_resets: 2
    auto_approve_when_all_benched: true
    wrong_branch_rescue: true
    strike_warning: true
```

## 5. Prioritised backlog

| #   | Item                                                                                                                        | MoSCoW | Size |
| --- | --------------------------------------------------------------------------------------------------------------------------- | ------ | ---- |
| 1   | Bench provenance (`source`, `task_id`, reset counter) with backward-compatible loading                                      | Must   | S    |
| 2   | Moderate-bar check helper (commits ahead plus verification pass), reusing completion verification                           | Must   | S    |
| 3   | Reset path: todo with note, unbench, avoid-engineer hint, reset cap, `blocked` plus escalation                              | Must   | M    |
| 4   | All-benched detection, auto-approval into the merge queue, and audit trail                                                  | Must   | M    |
| 5   | Timed unbench of `auto_park` entries after the cool-off                                                                     | Must   | S    |
| 6   | Strike warning one strike before parking                                                                                    | Should | S    |
| 7   | Wrong-branch rescue (cherry-pick, abort on conflict)                                                                        | Should | M    |
| 8   | Task-scoped effort override, with relaunch on assignment and revert on completion                                           | Should | M    |
| 9   | `workflow_policy.bench_recovery` config, docs (`docs/config-reference.md`, `docs/reference/agent-efficiency.md`), changelog | Must   | S    |
| 10  | `batty status` shows bench source, time left in the cool-off, and the task's reset count                                    | Could  | S    |

## 6. Risks and mitigations

| Risk                                                                              | Mitigation                                                                                                                                                                                                                                     |
| --------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Auto-approval merges work no human reviewed, while `auto_merge` itself is opt-in. | Approve only when **all** engineers are benched and the moderate bar passes. Record `approved_by: daemon` everywhere. Users can turn it off with `auto_approve_when_all_benched: false`. Call this out in the changelog as a behaviour change. |
| Scope is ignored, so out-of-scope edits can merge.                                | This was a deliberate choice. Keep the scope check result in the approval record so it can be audited; strict mode can be added later.                                                                                                         |
| Reset loops burn tokens, especially at raised effort.                             | Hard cap of 2 resets per task, then `blocked` plus escalation. The effort bump is per task and reverts.                                                                                                                                        |
| Wrong-branch rescue cherry-picks unrelated commits.                               | Only take the engineer's own commits made after the assignment, on their own worktree branches. Abort on conflict. Record an event listing the commits.                                                                                        |
| A manual bench is undone by automation.                                           | Every automatic action checks `source == auto_park`. Existing entries load as `manual`.                                                                                                                                                        |
| An effort relaunch loses agent context.                                           | Relaunch only when a reset task is assigned, which is already a fresh start.                                                                                                                                                                   |
| On-by-default changes existing teams.                                             | Note it in `docs/migrating-to-0.12.md` with the one-line opt-out.                                                                                                                                                                              |

## 7. Integration points

- `src/team/bench.rs`: provenance fields and loading.
- `src/team/daemon/health/poll_shim.rs`: strike warning and wrong-branch rescue before the park.
- `src/team/daemon/dispatch/queue.rs`: avoid-engineer hint, effort override at assignment, all-benched detection.
- `src/team/merge/completion/` (`gates.rs`, `verify_job.rs`): the moderate-bar check and the approval route into the merge queue.
- `src/team/daemon/automation/` (`reviews.rs`, `housekeeping.rs`): the timed unbench and the all-benched pass.
- `src/team/agent_options.rs` / launcher: the effort ladder and per-task override.
- `src/team/config/types.rs`: `BenchRecoveryConfig` under `workflow_policy`.

## 8. Next steps

1. Break items 1–9 into board tasks, one per row. Items 1 and 2 go first, since everything else depends on them.
1. Write the tests up front for each path: provenance compatibility, the all-benched approval with the bar passing and failing, the reset cap, the timed unbench ignoring manual benches, and the effort ladder at its top.
1. Remaining question for the owner: should the hourly "team fully benched (manual)" alert go to the configured human channel only, or also to the architect?
