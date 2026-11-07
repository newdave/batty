# Task Breakdown: Bench recovery automation

**Source PRP**: [docs/prps/bench-recovery-automation.md](../prps/bench-recovery-automation.md)
**Brainstorm**: [docs/brainstorming/2026-11-07-bench-recovery-automation.md](../brainstorming/2026-11-07-bench-recovery-automation.md)
**Template**: [docs/templates/technical-task-template.md](../templates/technical-task-template.md)

## Feature Overview

Today an engineer parked for unattributed completions (`zero_diff_recovery: park`) stays benched until a human acts, so a fully benched team stalls. This feature does five things:

- Records where each bench came from.
- Rescues work committed on the wrong branch, and warns the engineer one strike before parking.
- Unbenches daemon-parked engineers after a cool-off. The parked task is either sent on to merge or reset to another engineer at a higher effort, capped at two resets.
- Auto-approves `review` tasks that meet a moderate bar when every dispatchable engineer is benched.
- Sends an hourly human alert when the whole team is benched by hand.

All of it sits under `workflow_policy.bench_recovery`, on by default.

The 12 tasks below map one-to-one onto the PRP's "List of tasks, in order". None is split. Each is sized for one PR: S is under about 200 changed lines, M is about 200 to 600.

## Shared Rules (apply to every task)

- **Stack**: Rust, no new dependencies in `Cargo.toml`.
- **Tests**: every change gets `#[cfg(test)]` coverage. Unit tests run without tmux. Integration tests are tmux-gated with `#[cfg_attr(not(feature = "integration"), ignore)]`. Most tests here are unit tests that use `TestDaemonBuilder`, `init_git_repo` and `write_bench_test_team_config`.
- **Bench lock**: `with_bench_lock` has a 2s timeout. Never run git, send messages or write the board inside its closure.
- **Builder default**: `TestDaemonBuilder` sets `bench_recovery.enabled = false` (from BR-02 on). New tests must opt in with `.workflow_policy(..)`.
- **Validation commands** (every task; swap the filter for the one listed per task):

```bash
export CARGO_TARGET_DIR=target
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib <filter>
cargo test --lib            # full lib suite before merge
```

- **Definition of Done**: all validation commands green; new tests cover happy path, edge cases and failures; every automated action emits one event and one `record_orchestrator_action` line; no `Manual` bench entry is ever touched automatically.

---

## BR-01: Add bench provenance (source and task_id)

**Priority**: Critical | **Size**: S | **Depends on**: none | **Parallel with**: BR-02

**Purpose**: As the daemon, I need to know whether a bench was manual or an auto-park, and for which task, so that automation never touches an operator's bench.

**Files**
```
├── src/team/bench.rs - BenchSource enum; BenchEntry.source/task_id with #[serde(default)];
│                       bench_engineer_with(); bench_engineer() becomes a Manual wrapper;
│                       pub fn bench_entries(root) -> Result<BTreeMap<String, BenchEntry>>;
│                       format_benched_engineers_section shows "(auto, task #N)" for AutoPark
└── src/team/status/tests.rs:2375 - update the BenchEntry struct literal
```

**Patterns**: `BenchState`/`BenchEntry` (bench.rs:21-32); `bench_engineer` (50-67); temp file + rename write (121-149).

**Acceptance Criteria**
```gherkin
Scenario: Legacy bench file loads as manual
  Given a bench JSON entry with only "timestamp" and "reason"
  When bench_entries(root) loads it
  Then the entry has source == BenchSource::Manual and task_id == None

Scenario: Auto-park provenance persists
  Given a team config written with write_bench_test_team_config
  When bench_engineer_with(root, "eng-1", Some(..), BenchSource::AutoPark, Some(42)) runs
  Then the bench file contains "source": "auto_park" and "task_id": 42
  And format_benched_engineers_section contains "(auto, task #42)"

Scenario: Manual wrapper unchanged
  When bench_engineer(root, "eng-1", None) runs
  Then the stored entry has "source": "manual" and no task_id key
```

**Tests (unit)**: legacy JSON loads as Manual; `bench_engineer_with` round-trip; status formatting for AutoPark; the existing concurrency test still passes.

**Validation filter**: `cargo test --lib bench`

**Risks**: The 2s bench lock means `bench_entries` must release the lock before returning. `validate_engineer` reads team.yaml from disk, so tests must write a team config first.

---

## BR-02: Add BenchRecoveryConfig to WorkflowPolicy

**Priority**: Critical | **Size**: S | **Depends on**: none | **Parallel with**: BR-01

**Purpose**: As an operator, I need one config block with safe defaults and an opt-out.

**Files**
```
├── src/team/config/types.rs - BenchRecoveryConfig + EffortLadders (mirror MainSmokePolicy 488-514);
│                              #[serde(default)] pub bench_recovery on WorkflowPolicy + Default (403-450)
├── src/team/config/tests_advanced.rs - defaults assertions (503-530 pattern)
└── src/team/test_support.rs - TestDaemonBuilder sets workflow_policy.bench_recovery.enabled = false
```

**Acceptance Criteria**
```gherkin
Scenario: Defaults
  Given a team.yaml with no bench_recovery key
  When the config is parsed
  Then workflow_policy.bench_recovery has enabled: true, cooloff_secs: 1800, max_auto_resets: 2,
    auto_approve_when_all_benched: true, wrong_branch_rescue: true, strike_warning: true
  And effort_ladder.claude == [low, medium, high, xhigh, max]
  And effort_ladder.codex == [minimal, low, medium, high, xhigh, max]

Scenario: Partial override
  Given bench_recovery: { cooloff_secs: 60 }
  When parsed
  Then cooloff_secs == 60 and every other field keeps its default

Scenario: Test builder opt-out
  When TestDaemonBuilder::new(..).build() runs without .workflow_policy
  Then daemon policy bench_recovery.enabled == false
```

**Tests (unit)**: defaults, partial override, builder default.

**Validation filter**: `cargo test --lib config`, then the full `cargo test --lib` to confirm that no existing test changes behaviour.

**Risks**: `TestDaemonBuilder` defaults to `WorkflowPolicy::default()`. Without the builder opt-out, existing all-benched tests change behaviour. The ladders are not verified against the CLIs: check `claude --help` (`--effort`) and the Codex `model_reasoning_effort` docs, and keep them configurable.

---

## BR-03: Create pure bench_recovery helpers

**Priority**: Critical | **Size**: M | **Depends on**: BR-02 (ladder types) | **Parallel with**: none (foundation)

**Purpose**: As the daemon, I need persisted per-task recovery state and a single bar check, so that restarts keep state and both automated paths share one safety bar.

**Files**
```
├── src/team/bench_recovery.rs (CREATE) - BenchRecoveryFrontmatter { bench_reset_count, avoid_engineers,
│     effort_override, bench_reset_capped }; read_bench_recovery / write_bench_recovery;
│     next_effort; BarEvidence; bar_commits_met; attributed_commit_count; verification_meets_bar
└── src/team/mod.rs - pub mod bench_recovery;
```

**Patterns**: `WorkflowFrontmatter` / `read_workflow_metadata` (board.rs:20-60, 117-123); writes go through `task_cmd::update_task_frontmatter` (864-875, Mapping-based). Do not add fields to `Task`, which has about 40 struct literals.

**Acceptance Criteria**
```gherkin
Scenario: Effort ladder
  Given ladder [low, medium, high, xhigh, max]
  Then next_effort(ladder, Some("medium")) == Some("high")
  And next_effort(ladder, None) == Some("high")          # base is "medium"
  And next_effort(ladder, Some("max")) == Some("max")     # capped at top
  And next_effort(ladder, Some("bogus")) == None
  And next_effort(&[], Some("low")) == None

Scenario: Auto-save commits are not attributed
  Given an init_git_repo fixture with eng-main/eng-1 and a task branch holding one real commit
    and one "wip: auto-save ... [batty]" commit
  When attributed_commit_count(worktree, "eng-main/eng-1", task_branch) runs
  Then it returns 1

Scenario: Skipped verification is not a pass
  Given a skipped_verification_run result (passed: true) and auto_run_tests == false
  Then verification_meets_bar returns false
  And a timed_out_verification_run result also returns false
  And a real passing run with auto_run_tests == true returns true

Scenario: Frontmatter round-trip
  Given a task file with unrelated key "priority: high"
  When write_bench_recovery sets bench_reset_count: 1 and avoid_engineers: [eng-1]
  Then read_bench_recovery returns those values and "priority: high" is unchanged
```

**Tests (unit)**: all four scenarios above, plus `bar_commits_met` truth table (0/0, 1/0, 0/1, 1/1).

**Validation filter**: `cargo test --lib bench_recovery`

**Risks**: Daemon `[batty]` commits count as commits ahead. Excluding them is the core of the bar. Skipped verification must never count as a pass.

---

## BR-04: Write auto_park source on zero-diff park

**Priority**: High | **Size**: S | **Depends on**: BR-01, BR-03 | **Parallel with**: BR-05 to BR-08

**Files**
```
└── src/team/daemon/health/poll_shim.rs:713-717 - bench_engineer_with(.., Some("completion attribution
      requires operator review"), BenchSource::AutoPark, Some(task_id))
```

**Acceptance Criteria**
```gherkin
Scenario: Park writes provenance
  Given a TestDaemonBuilder with .workflow_policy(zero_diff_recovery: Park) and an assigned task #7
  When eng-1 reports ZERO_DIFF_COMPLETION_RECOVERY_THRESHOLD (2) completions with no attributed commit
  Then the bench file entry for eng-1 has source "auto_park" and task_id 7
```

**Tests (unit)**: this is the first Park-mode test in the codebase. Use the zero-diff fixtures in daemon/tests.rs:862-1070 and `prepare_engineer_assignment_worktree`.

**Validation filter**: `cargo test --lib zero_diff`

**Risks**: The builder defaults to `Restart`, so the test must set `Park` explicitly.

---

## BR-05: Send a strike warning one strike before parking

**Priority**: Medium | **Size**: S | **Depends on**: BR-03 | **Parallel with**: BR-04, BR-06 to BR-08

**Files**
```
├── src/team/daemon/health/poll_shim.rs - after note_zero_diff_completion (681): if enabled &&
│     strike_warning && count == THRESHOLD - 1, queue_daemon_message(member, text)
└── src/team/events.rs - TeamEvent::completion_attribution_warning (pattern: task_escalated 415-422)
```

**Acceptance Criteria**
```gherkin
Scenario: Warning in Park mode
  Given bench_recovery.enabled and strike_warning are true and zero_diff_recovery is Park
  When eng-1 records its first zero-diff strike (count 1, THRESHOLD 2)
  Then eng-1 receives a daemon message that mentions parking
  And the event log contains "completion_attribution_warning"
  And an orchestrator log line is recorded

Scenario: Restart wording
  Given zero_diff_recovery is Restart
  Then the message mentions a restart and does not contain "parks you"

Scenario: Disabled
  Given strike_warning: false (or enabled: false)
  Then no message and no completion_attribution_warning event
```

**Tests (unit)**: all three scenarios.

**Validation filter**: `cargo test --lib zero_diff`

**Risks**: Merge conflicts with BR-04 and BR-06 in the same function. Land them one after another, or rebase carefully.

---

## BR-06: Rescue commits made on the wrong branch

**Priority**: Medium | **Size**: M | **Depends on**: BR-03 | **Parallel with**: BR-04, BR-05, BR-07, BR-08

**Files**
```
├── src/team/daemon/health/poll_shim.rs - before the strike (681): find_rescuable_commits + cherry-pick
│     via crate::git::cmd; abort_in_progress_git_operations on conflict; re-count
└── src/team/events.rs - TeamEvent::completion_branch_rescued
```

**Patterns**: `branch_belongs_to_engineer` (worktree/ops.rs:86-90), `abort_in_progress_git_operations` (709-716). There is no existing cherry-pick helper.

**Acceptance Criteria**
```gherkin
Scenario: Rescue from eng-main
  Given wrong_branch_rescue is true and eng-1 committed "feat: x" on eng-main/eng-1 after assignment
  When the completion is tracked and the attributed count is 0
  Then the commit is cherry-picked onto the task branch
  And "completion_branch_rescued" is emitted with the commit list
  And no zero-diff strike is recorded for the task

Scenario: Conflict aborts
  Given the rescued commit conflicts with the task branch
  Then the cherry-pick is aborted, the worktree has no in-progress cherry-pick
  And the strike is recorded as before

Scenario: Other engineers untouched
  Given eng-2 has new commits on its own branches
  Then no eng-2 commit is selected or cherry-picked
```

**Tests (unit, git fixtures)**: the three scenarios, plus "commits from before the assignment are ignored" and "`[batty]` commits are excluded".

**Validation filter**: `cargo test --lib zero_diff`

**Risks**: Commit selection is the PRP's highest-risk piece. Limit it to commits created after the assignment time and to the engineer's own branches. Exclude `[batty]` subjects. Ship behind `wrong_branch_rescue`.

---

## BR-07: Apply effort_override at launch and dispatch

**Priority**: Medium | **Size**: M | **Depends on**: BR-03 | **Parallel with**: BR-04 to BR-06, BR-08

**Files**
```
├── src/team/agent_options.rs resolve() (80-98) - use the active task's effort_override when
│     init::backend_supports_effort(backend); otherwise debug log and keep configured effort
├── src/team/daemon/dispatch/queue.rs - after active_tasks insert (1640): if override differs from
│     .batty/launch-options/<member>.json effort, handle_shim_cold_respawn(member, "bench_recovery_effort")
├── merge landing / completion path - clear effort_override when the task reaches done
└── src/team/daemon/health/preflight.rs:113-125 - compare against the override-aware effort
```

**Patterns**: `agent_options::all` errors for unsupported backends (54-64); effort is used at launcher.rs:680-721 and shim_spawn.rs:134-152.

**Acceptance Criteria**
```gherkin
Scenario: Override on Claude
  Given eng-1 (claude, effort: medium) is assigned task #7 with effort_override: high
  When resolve(root, eng-1) runs
  Then effort == "high"
  And handle_shim_cold_respawn is called once with reason "bench_recovery_effort"

Scenario: No relaunch when effort is unchanged
  Given the recorded launch effort is already "high"
  Then no cold respawn happens

Scenario: Unsupported backend
  Given a member whose backend lacks effort support
  Then resolve returns the configured effort, logs at debug, and returns no error

Scenario: Preflight
  Given the active task has effort_override: high and the launch file records "high"
  Then preflight does not report "launch settings missing or changed"

Scenario: Cleared on done
  When task #7 reaches done
  Then its frontmatter no longer has effort_override
```

**Tests (unit)**: all five scenarios.

**Validation filter**: `cargo test --lib dispatch` and `cargo test --lib agent_options`

**Risks**: `agent_options::all` errors when effort is set on an unsupported backend, so check support before applying the override. Preflight will report false mismatches if it does not know about the override. Ship behind `enabled`.

---

## BR-08: Move avoid_engineers to the back of dispatch ranking

**Priority**: High | **Size**: S | **Depends on**: BR-03 | **Parallel with**: BR-04 to BR-07

**Files**
```
└── src/team/daemon/dispatch/queue.rs enqueue_dispatch_candidates after rank (1078-1084) - stable-move
      avoided engineers to the back, mirroring the retry-owner reorder (1085-1100)
```

**Acceptance Criteria**
```gherkin
Scenario: Avoided engineer ranked last
  Given task #7 has avoid_engineers: [eng-1] and eng-1, eng-2 are idle
  When enqueue_dispatch_candidates runs
  Then eng-2 is ranked ahead of eng-1

Scenario: Fallback to the same engineer
  Given eng-1 is the only idle engineer
  Then task #7 is still dispatched to eng-1
```

**Tests (unit)**: both scenarios. Follow the pattern of `enqueue_dispatch_candidates_skips_benched_engineer` (4170-4190).

**Validation filter**: `cargo test --lib dispatch`

**Risks**: A pinned `assignee:` is a hard filter (queue.rs:1745-1749). The reorder cannot override it. BR-09 must clear it.

---

## BR-09: Implement the reset path and protect capped tasks

**Priority**: Critical | **Size**: M | **Depends on**: BR-03 (and BR-01 for the unbench) | **Parallel with**: BR-04 to BR-08

**Files**
```
├── src/team/daemon/bench_recovery.rs (CREATE) - impl TeamDaemon { fn reset_parked_task(engineer, task_id) }
├── src/team/daemon.rs (or daemon/mod) - mod bench_recovery;
├── src/team/events.rs - task_bench_reset, task_bench_reset_capped
└── src/team/daemon/automation/aging.rs:341-400 - maybe_auto_unblock_blocked_tasks skips bench_reset_capped
```

**Patterns**: `reclaim_task_claim_with_attribution` (task_cmd.rs:296-320), `block_task_with_reason_and_attribution` (119-134), `block_escalated_task` (gates.rs:385-418), `archive_branch_if_commits_ahead` (worktree/ops.rs:308-339), `clear_active_task` (daemon.rs:1562), `record_task_escalated` (telemetry.rs:500-526).

**Acceptance Criteria**
```gherkin
Scenario: First and second reset
  Given task #7 is in progress and pinned with assignee: eng-1, bench_reset_count: 0, max_auto_resets: 2
  When reset_parked_task("eng-1", 7) runs
  Then the task status is todo and assignee, branch, worktree_path, claimed_by are cleared
  And bench_reset_count == 1, avoid_engineers == [eng-1], effort_override == "high" (from "medium")
  And the old branch exists under preserved/*
  And eng-1 is no longer in the bench file
  And "task_bench_reset" is emitted and one orchestrator line is recorded
  And a second reset gives bench_reset_count == 2 and effort_override one step higher

Scenario: Cap reached
  Given bench_reset_count == 2
  When reset_parked_task runs
  Then the task is blocked with blocked_on "bench recovery: reset cap reached" and bench_reset_capped: true
  And record_task_escalated fires and the RoleType::User roles receive a message
  And "task_bench_reset_capped" is emitted and the engineer is unbenched

Scenario: Auto-unblock skips capped tasks
  Given a blocked task with bench_reset_capped: true whose depends_on are all done
  When maybe_auto_unblock_blocked_tasks runs
  Then the task stays blocked
```

**Tests (unit)**: three scenarios, plus "zero-diff counters are cleared after reset" and "the archived branch is not returned by task_branch_candidates".

**Validation filter**: `cargo test --lib bench_recovery` and `cargo test --lib aging`

**Risks**:
- `Review -> Todo` is not a legal transition. Use `reclaim_task_claim_with_attribution`, which writes todo without a transition check, or go through Blocked.
- The pinned `assignee:` must be cleared, because `reclaim` does not clear it.
- Auto-unblock would undo the cap block unless it skips capped tasks.
- Archived branches must use `preserved/*` names so that `task_branch_candidates` (completion_checks.rs:285-321) ignores them.
- Do not hold the bench lock while archiving or writing the board.

---

## BR-10: Add the approval job and MergeOrigin::BenchRecovery

**Priority**: Critical | **Size**: M | **Depends on**: BR-09 | **Parallel with**: BR-12 drafting

**Files**
```
├── src/team/daemon/background_tests.rs - BenchApprovalJob, BenchApprovalKind { ApproveReview, RecoverParked },
│     bench_approvals: HashMap<u32, BenchApprovalJob>
├── src/team/daemon/merge_queue.rs - MergeOrigin::BenchRecovery; landing branch (1474-1482) approves with
│     reviewer "daemon (all engineers benched)" via StatusTransitionAttribution::daemon (mirror
│     approve_chat_merge 2643-2677); telemetry branch (1498-1516)
├── src/team/daemon/bench_recovery.rs - start_bench_approval_job, poll_bench_approval_jobs
└── src/team/events.rs - task_auto_approved
```

**Patterns**: chat merge (chat/commands.rs:222-369), `ChatMergeJob` (background_tests.rs:47-52), `TestJob::spawn_with_timeout` and `run_automatic_verification_tracked` (completion_checks.rs:65-97), `enqueue_reviewed_merge` (execute.rs:108-151).

**Acceptance Criteria**
```gherkin
Scenario: Review approval
  Given all engineers are benched, auto_merge.enabled is true, task #7 is in review, and the bar evidence passes
  When the ApproveReview job finishes with a real passing run
  Then a MergeRequest with origin BenchRecovery is enqueued
  And on landing the task records approved_by "daemon (all engineers benched)"
  And "task_auto_approved" is emitted

Scenario: auto_merge disabled
  Given workflow_policy.auto_merge.enabled == false
  Then no job is started and nothing is enqueued

Scenario: Engineer unbenched mid-job
  Given eng-2 is unbenched (batty unbench) before the job finishes
  Then nothing is enqueued

Scenario: Failed run
  Given the job finishes with a failing, skipped or timed-out run
  Then reset_parked_task runs (through Blocked for review tasks) and nothing is enqueued

Scenario: RecoverParked pass
  Given a RecoverParked job passes the bar
  Then enqueue_reviewed_merge(origin = BenchRecovery) runs and the engineer is unbenched
```

**Tests (unit)**: the five scenarios, plus "multi-repo refused" and "a duplicate job for the same task is not started". Pattern: `telegram_merge_command_routes_through_merge_queue` (chat/telegram.rs:1456-1512).

**Validation filter**: `cargo test --lib merge_queue` and `cargo test --lib bench_recovery`

**Risks**:
- Do not use `start_verification_job`: it is keyed by engineer and is dropped when the active task changes.
- Do not re-run `decide_merge_route`.
- Run verification on the task branch before the claim is released. It blocks, so it must stay inside `TestJob`.
- Re-read the bench state just before enqueueing.
- A skipped run counts as not passed.

---

## BR-11: Add the orchestration step, timed unbench and hourly alert

**Priority**: Critical | **Size**: M | **Depends on**: BR-04, BR-09, BR-10 | **Parallel with**: BR-12 drafting

**Files**
```
├── src/team/daemon/bench_recovery.rs - maybe_run_bench_recovery, all_engineers_benched
├── src/team/daemon/scheduler.rs - last_bench_recovery, last_full_bench_alert: Option<Instant>
├── src/team/daemon/poll.rs tick() - run_recoverable_step("bench_recovery", ..) after
│     process_merge_queue (332-338), before maybe_auto_dispatch (344-353)
└── src/team/events.rs - bench_auto_unbenched, team_fully_benched
```

**Acceptance Criteria**
```gherkin
Scenario: Manual bench untouched
  Given eng-1 is benched with source manual 2 hours ago
  When maybe_run_bench_recovery runs
  Then eng-1 stays benched and no job is started

Scenario: Cool-off
  Given eng-1 has an auto_park entry with timestamp 10 minutes ago, cooloff_secs 1800, and eng-2 is not benched
  Then no action is taken
  And when the timestamp is 31 minutes ago, a RecoverParked job starts for the entry's task_id

Scenario: Missing or inactive task
  Given an auto_park entry with no task_id
  Then eng-1 is unbenched and "bench_auto_unbenched" is emitted

Scenario: All benched ignores on_demand
  Given eng-1 and eng-2 (dispatchable) are benched and eng-3 has an on_demand role and is not benched
  Then all_engineers_benched returns true and auto_park entries are handled without waiting for the cool-off

Scenario: Hourly manual alert
  Given every dispatchable engineer is benched with source manual
  When the step runs twice within one hour
  Then the RoleType::User roles receive exactly one alert and "team_fully_benched" is emitted once

Scenario: Non-git no-op
  Given a non-git builder with enabled: true
  Then no job, unbench or reset happens (only the alert may fire)

Scenario: Throttle
  Given the step ran under 30s ago
  Then it returns without reading the bench file
```

**Tests (unit)**: all seven scenarios.

**Validation filter**: `cargo test --lib bench_recovery`, then the full `cargo test --lib`.

**Risks**:
- Use a dedicated `Option<Instant>` for the alert, not `suppress_recent_escalation`, because that prunes every key with the caller's window.
- Call `bench_entries` once and release the lock before doing any work.
- An empty dispatchable set must return false.
- Multi-repo projects must stay a no-op.

**Integration test (tmux-gated)**: add one `#[cfg_attr(not(feature = "integration"), ignore)]` smoke test that runs a full `tick()` with a parked engineer and checks the step runs without error. Run it with `env -u TMUX TMUX_TMPDIR=$(mktemp -d) cargo test --lib --features integration bench_recovery`.

---

## BR-12: Document the feature and the on-by-default change

**Priority**: High | **Size**: S | **Depends on**: BR-01 to BR-11 (content), can be drafted earlier

**Files**
```
├── docs/config-reference.md - bench_recovery field list (mirror main_smoke at 262-271)
├── docs/reference/agent-efficiency.md - bench recovery paragraph
├── docs/migrating-to-0.12.md - on by default; opt out with workflow_policy.bench_recovery.enabled: false
└── CHANGELOG.md - 0.12.0 Added (bench recovery, new events) and Changed (on by default)
```

**Acceptance Criteria**
```gherkin
Scenario: Config documented
  Given docs/config-reference.md
  Then every BenchRecoveryConfig field appears with its default (enabled, cooloff_secs, max_auto_resets,
    auto_approve_when_all_benched, wrong_branch_rescue, strike_warning, effort_ladder)

Scenario: Migration note
  Given docs/migrating-to-0.12.md
  Then it states the feature is on by default and shows the opt-out YAML
```

**Validation**: `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --lib`. Run `./scripts/generate-docs.sh` only if the CLI help changed; it should not. Never hand-edit `docs/reference/cli.md`.

---

## Dependency Graph and Critical Path

```text
BR-01 (provenance) ─┐
                    ├─> BR-03 (pure helpers) ─┬─> BR-04 (auto_park) ───────────────┐
BR-02 (config) ─────┘                         ├─> BR-05 (warning)                   │
                                              ├─> BR-06 (rescue)                    │
                                              ├─> BR-07 (effort override)           │
                                              ├─> BR-08 (avoid list)                │
                                              └─> BR-09 (reset) ─> BR-10 (approval) ┴─> BR-11 (step) ─> BR-12 (docs)
```

- **Foundations**: BR-01 and BR-02 can run in parallel. BR-03 needs both. The PRP orders them 1, 2, 3.
- **Critical path**: BR-01/BR-02, then BR-03, BR-09, BR-10, BR-11, BR-12. BR-04 must also land before BR-11, because without `auto_park` entries the step has nothing to act on.
- **Parallel after BR-03**: BR-04 to BR-08 and BR-09. BR-04, BR-05 and BR-06 all edit `maybe_track_zero_diff_completion` / `maybe_recover_zero_diff_completion_loop` in `poll_shim.rs`. Give them to one engineer, or land them in order 4, 5, 6 to avoid conflicts. BR-07 and BR-08 both touch `dispatch/queue.rs`, but in different functions.
- **Bottlenecks**: BR-10 (the async job plus a new merge origin) and BR-06 (no existing cherry-pick helper).
- **De-scope option**: per the PRP, BR-06 and BR-07 can ship later behind their flags without blocking the critical path. BR-09 then writes `effort_override`, but nothing applies it until BR-07 lands. This is harmless.

## Implementation Recommendations

- **Team**: three engineers.
  - A: BR-01, BR-03, BR-09, BR-10, BR-11 (the critical path).
  - B: BR-02, then BR-04, BR-05, BR-06 (`poll_shim.rs`).
  - C: BR-08, BR-07, BR-12.
- **Sequencing**: merge BR-02 early. Its builder opt-out keeps the existing suite stable for every later PR. Run the full `cargo test --lib` (about 4.1k tests) on each PR, not just the filter.
- **Manual check before release**: in a scratch repo, run `batty init --template simple` and set `zero_diff_recovery: park`.
  1. Bench all engineers by hand. Expect only the hourly alert.
  2. Simulate an `auto_park` entry. Confirm the reset or approval in `batty status` and in the orchestrator log.
