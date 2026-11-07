name: "PRP: Bench recovery automation"
description: |
  Automate benching and unbenching of engineers so a batty team keeps moving when
  engineers are parked for unattributed completions. When every engineer is
  benched, the daemon approves review work that meets a moderate bar and resets
  parked tasks onto other engineers at a higher effort.

## Purpose

AI implementation guide with complete context, validation commands and patterns to mirror, so the feature can be built in one pass. Follow all rules in `CLAUDE.md`. Every module change ships with `#[cfg(test)]` coverage, and `cargo test` must be green before committing.

Task breakdown: [docs/tasks/bench-recovery-automation.md](../tasks/bench-recovery-automation.md)

---

## Discovery Summary

### Initial Task Analysis

The request was to "add additional automation around avoiding workers being benched/unbenched, and ensure task approval is automatic if all engineers are benched".

Engineers are benched in two ways today. The first is by hand with `batty bench`. The second is automatic: with `workflow_policy.zero_diff_recovery: park`, which is the default, an engineer is parked after `ZERO_DIFF_COMPLETION_RECOVERY_THRESHOLD` (2) completions that have no attributed commit on the task branch.

Nothing ever unbenches automatically. Once every engineer is parked, nobody can rework or approve anything, and the run stalls until a human steps in. The design came out of a facilitated brainstorm: `docs/brainstorming/2026-11-07-bench-recovery-automation.md`.

### User Clarifications Received

Brainstorm session (owner decisions):

- **Question:** Which approval becomes automatic? **Answer:** Both the operator review needed to unbench a parked engineer, and approval of tasks in `review`. **Impact:** two automated paths share one safety bar.
- **Question:** What safety bar applies? **Answer:** Moderate: commits ahead plus passing verification; the scope fence is ignored. **Impact:** one shared `meets_bench_bar` check.
- **Question:** What happens to a parked task that fails the bar? **Answer:** Up to 2 automatic resets. The engineer is unbenched, the work goes to a different engineer, and effort goes up one level. **Impact:** a reset path, an avoid list, and an effort override.
- **Question:** Is the effort bump scoped to the task or the engineer? **Answer:** The task, and it falls back to the same engineer if no one else is free. **Impact:** the effort override is stored on the task and applied when the task is assigned.
- **Question:** Which prevention measures? **Answer:** Wrong-branch rescue, a strike warning, and a timed unbench. Auto-commit of uncommitted work is out of scope. **Impact:** three prevention steps.
- **Question:** What scope, cool-off and default apply to timed unbench? **Answer:** Daemon-applied benches only (manual benches are never touched), a 30-minute cool-off, and on by default. **Impact:** bench provenance, `cooloff_secs: 1800`, `enabled: true`.

Preflight gaps. The owner re-ran PRP generation without objecting, so the recommended defaults were adopted:

- **Question:** What does "all engineers benched" mean? **Answer:** Every *dispatchable* engineer member (one whose role is not `on_demand`) is in the bench file. Offline or crashed engineers count as not benched. **Impact:** conservative trigger for auto-approval.
- **Question:** "Commits ahead" of what? **Answer:** Ahead of trunk (multi-repo aware), plus at least one commit attributed to the engineer from the engineer base branch to the task branch, excluding daemon `[batty]` auto-save commits. **Impact:** a WIP auto-save cannot pass the bar.
- **Question:** What counts as "verification passes"? **Answer:** A real verification run (`verification.test_command`, then `workflow_policy.test_command`, then the framework default). A skipped run (`auto_run_tests: false`) or a timeout does **not** pass. **Impact:** the bar cannot silently degrade to commits only.
- **Question:** How does auto-approval interact with the `auto_merge` gates? **Answer:** A task only reaches `review` after failing the auto-merge gates (or by override), so re-running those gates would approve nothing. **Decision:**
  - If `workflow_policy.auto_merge.enabled == false`, the daemon never auto-approves. It alerts the human instead.
  - Otherwise, approval works like a human chat merge (`$merge`): run the tests, then `enqueue_reviewed_merge` with a new origin, and do not re-run the gates.
  - "All benched" is re-checked just before enqueueing. **Impact:** reuses the chat-merge path.
- **Question:** What happens to the branch and worktree on reset? **Answer:**
  - The old task branch is archived with the existing `preserved/*` helpers.
  - The next engineer starts a fresh task branch from trunk through normal dispatch.
  - The claim, `assignee`, `branch` and `worktree_path` are cleared, and so are the in-memory strike counters.
  - The reset count, avoid list and effort override live in task frontmatter. **Impact:** state survives restarts.
- **Question:** How do effort edge cases behave? **Answer:**
  - When a member has no configured effort, its base is the backend default `medium`.
  - Backends without effort support skip the override with a log and never error.
  - Only the receiving member is relaunched, and only if its effective effort changes.
  - The ladder is capped at its top step. **Impact:** a non-failing override path.
- **Question:** Do the prevention steps run under `zero_diff_recovery: restart`? **Answer:** Yes. Rescue and warning run in both modes, and the warning omits "parks you" in restart mode. Timed unbench and reset act only on `auto_park` entries. **Impact:** prevention is independent of the recovery mode.
- **Question:** How do existing bench entries behave? **Answer:** They load as `manual` (`#[serde(default)]`). The cool-off uses the persisted bench timestamp. **Impact:** upgrades never unbench anyone unexpectedly.
- **Question:** Where does the hourly "team fully benched (manual)" alert go? **Answer:** To the configured human (`RoleType::User`) channel only. **Impact:** a human-only notification.

### Missing Requirements Identified

- Zero-diff strike tracking is disabled outside single-repo git (`poll_shim.rs:653`), so `auto_park` never occurs there. `enqueue_reviewed_merge` also refuses multi-repo projects. **Requirement:** bench recovery does nothing in non-git or multi-repo projects, apart from the hourly alert.
- The state machine forbids `Review -> Todo` (`workflow.rs:89-120`). **Requirement:** a review task that is reset goes through `Blocked` or `InProgress` first, or uses `reclaim_task_claim_with_attribution`, which writes `todo` without a transition check.
- `maybe_auto_unblock_blocked_tasks` (`automation/aging.rs:341-400`) would move a task blocked at the reset cap back to `todo`. **Requirement:** that function must skip tasks marked with the reset cap.
- No test covers the Park branch today; `TestDaemonBuilder` defaults to `Restart`. **Requirement:** add Park tests.
- Turning the feature on by default affects existing tests. **Requirement:** `TestDaemonBuilder` sets `bench_recovery.enabled = false` by default, and the new tests turn it on.

## Goal

When this ships:

1. Each bench records where it came from: `manual` or `auto_park`.
2. Before a strike counts, the daemon rescues work that was committed on the wrong branch.
3. One strike before parking, the engineer gets an explicit warning.
4. Daemon-parked engineers are unbenched after a 30-minute cool-off. Their parked task is either sent on, if it meets the bar, or reset to another engineer at one effort level higher. A task is reset at most twice; after that it is blocked and escalated.
5. When every dispatchable engineer is benched:
   - `auto_park` entries are handled immediately.
   - `review` tasks that meet the bar are approved and merged by the daemon, with `approved_by: daemon (all engineers benched)` recorded.
   - A team benched entirely by hand triggers an hourly human alert.

All of this sits under `workflow_policy.bench_recovery` and is on by default.

## Why

- **Throughput:** the project's priority order is stability, then throughput (`CLAUDE.md`). A fully benched team stops all flow until a human acts.
- **Fewer false parks:** a common cause of a park is real work committed on the wrong branch, which the rescue recovers.
- **Self-healing:** retries go to a different engineer at higher effort, with a hard cap and a clear escalation, and never touch an operator's manual bench.

## What

**User-visible behaviour:**
- `batty status` shows each bench's source and the time left in its cool-off. This is a Could item; at minimum the source must appear.
- New events: `completion_branch_rescued`, `completion_attribution_warning`, `bench_auto_unbenched`, `task_bench_reset`, `task_bench_reset_capped`, `task_auto_approved`, `team_fully_benched`.
- Every automated action writes a line to the orchestrator log via `record_orchestrator_action`.
- Config:

```yaml
workflow_policy:
  bench_recovery:
    enabled: true
    cooloff_secs: 1800
    max_auto_resets: 2
    auto_approve_when_all_benched: true
    wrong_branch_rescue: true
    strike_warning: true
    effort_ladder:
      claude: [low, medium, high, xhigh, max]
      codex: [minimal, low, medium, high, xhigh, max]
```

### Success Criteria

- [ ] Bench entries carry `source` and `task_id`. Old files load as `manual`. `batty bench` writes `manual` and the zero-diff park writes `auto_park`.
- [ ] Wrong-branch rescue cherry-picks the engineer's own new commits onto the task branch and re-counts before the strike. A cherry-pick conflict is aborted and the strike counts.
- [ ] A strike warning is sent when the count reaches `THRESHOLD - 1`, with wording that depends on the mode.
- [ ] Timed unbench acts only on `auto_park` entries older than `cooloff_secs`. Manual entries are never touched.
- [ ] Reset path:
  - A task that meets the bar is sent through to merge, then the engineer is unbenched.
  - Otherwise, if `bench_reset_count < max_auto_resets`: archive the branch, reclaim the task to `todo`, clear the claim fields, append the engineer to `avoid_engineers`, raise `effort_override` one step, and increment the count.
  - At the cap, move the task to `blocked` with `blocked_on`, escalate to the human channel, and unbench the engineer.
- [ ] Dispatch moves `avoid_engineers` to the back of the candidate list but never removes them. It applies `effort_override` to the receiving member and cold-respawns it only when its effective effort differs. Completion clears the override.
- [ ] All-benched pass:
  - `review` tasks that meet the bar are approved through a task-keyed test job and then `enqueue_reviewed_merge(origin = BenchRecovery)`, recording reviewer `daemon (all engineers benched)`.
  - Nothing is approved when `auto_merge.enabled == false` or `auto_approve_when_all_benched == false`.
  - A team benched entirely by hand alerts the human at most once an hour.
- [ ] Nothing runs in non-git or multi-repo projects, except the alert.
- [ ] `make lint` and `cargo test` pass. New tests cover every path above.
- [ ] Docs: `docs/config-reference.md`, `docs/reference/agent-efficiency.md`, `docs/migrating-to-0.12.md` (an on-by-default behaviour change, with the opt-out), and `CHANGELOG.md`.

## All Needed Context

### Research Phase Summary

- **Codebase patterns found:**
  - Bench state with a file lock (`team/bench.rs`).
  - Zero-diff strike and park (`poll_shim.rs`).
  - Dispatch ranking with a retry-owner reorder (`dispatch/queue.rs`).
  - Daemon-native approval to merge via a chat-merge test job (`chat/commands.rs`, `merge/completion/execute.rs`).
  - Branch archive helpers (`worktree/ops.rs`).
  - Task frontmatter metadata readers (`board.rs` `WorkflowFrontmatter`).
  - Poll-loop steps with throttles (`daemon/poll.rs`, `automation/housekeeping.rs`).
  - Escalation and human-channel delivery (`telemetry.rs`, `delivery/routing.rs`).
- **External research needed:** No. All patterns exist internally and no new dependencies are needed.
- **Knowledge gaps:**
  - There is no effort ladder or enum anywhere; effort is a free-form string, so a ladder must be added.
  - There is no cherry-pick helper; use `crate::git::cmd`.
  - Commits have no per-engineer author identity, so attribution is the `eng-main/<eng>..<task branch>` count minus `[batty]` commits.

### Documentation & References

```yaml
# MUST READ (paths relative to the repo root)
- file: src/team/bench.rs
  why: BenchState (21-25) and BenchEntry (27-32) get `source` and `task_id`; bench_engineer
    (50-67), unbench_engineer (69-78), with_bench_lock (151-154; 2s timeout, stale after 30s),
    validate_engineer reads team.yaml from disk (108-119), and write is temp file + rename (121-149).
- file: src/team/daemon/health/poll_shim.rs
  why: |
    ZERO_DIFF_COMPLETION_RECOVERY_THRESHOLD (26).
    maybe_track_zero_diff_completion (624-686): returns early when non-git or multi-repo (653),
    counts eng-main/<eng>..<task branch> (657-675), then strikes via note_zero_diff_completion (681).
    Insert the rescue before 681 and the warning after it.
    maybe_recover_zero_diff_completion_loop (688-747): park at 702-726, bench call at 713-717
    (switch it to source=auto_park, task_id).
    handle_shim_cold_respawn (749+) is the relaunch used for effort changes.
- file: src/team/daemon/health/poll_shim/shim_events.rs
  why: Call order 299-313 is track, then handle_engineer_completion, then recover.
- file: src/team/daemon/verification_tracking.rs
  why: In-memory zero_diff_completion_counts (12-58); note_/clear_zero_diff_completion (49-57).
- file: src/team/daemon/dispatch/queue.rs
  why: |
    enqueue_dispatch_candidates (870-1129): benched set loaded at 873, ranking at 1078-1084.
    The retry-owner reorder at 1085-1100 is the pattern for avoid_engineers (move to back, never remove).
    rank_dispatch_engineers (1728+): benched filter 1740; the pinned `assignee:` is a HARD
    filter at 1745-1749, so the reset must clear it.
    engineer_role_eligible (1696-1726): on_demand test at 1718-1725.
    process_dispatch_queue (1160-1691): claimed_by set at 1624, assign at 1634, active_tasks set at 1640.
    Apply the effort override after 1640.
    Tests: write_bench_test_team_config (2065-2075), enqueue_dispatch_candidates_skips_benched_engineer
    (4170-4190), unbench_restores_dispatch_eligibility (4237-4265).
- file: src/team/daemon/chat/commands.rs
  why: Template for daemon approval. Lines 222-283 check status == review, reject multi-repo,
    skip tasks already queued, and spawn TestJob::spawn_with_timeout(run_tests_in_worktree_tracked)
    into background_tests.chat_merges. poll_chat_merge_jobs / finish_chat_merge (286-369) re-check
    staleness, then call enqueue_reviewed_merge.
- file: src/team/daemon/background_tests.rs
  why: The ChatMergeJob struct (47-52) is the shape for a new BenchApprovalJob; test_job_timeout() is at 88-99.
- file: src/team/merge/completion/execute.rs
  why: enqueue_reviewed_merge (108-151) bails on multi-repo (115) and builds a MergeRequest with origin (148).
- file: src/team/daemon/merge_queue.rs
  why: MergeRequest (65-81); MergeOrigin (85-95), where BenchRecovery is added; enqueue_merge_request
    (952-960); board update by origin on landing (1474-1482); approve_chat_merge (2643-2677)
    hardcodes reviewer "telegram" and needs a BenchRecovery equivalent; telemetry by origin (1498-1516).
- file: src/team/merge/completion/decision.rs
  why: decide_merge_route (52-132). Do NOT re-run it for approval; read only `policy.enabled` (99-101).
- file: src/team/daemon/completion_checks.rs
  why: run_automatic_verification_tracked (65-97) is blocking and checks claim, task branch and scope
    first. Run it on the task branch before the claim is released. task_branch_candidates (285-321)
    must not treat archived reset branches as recovery branches.
- file: src/team/merge/completion/verify_job.rs
  why: Do NOT use start_verification_job (167-196): it is keyed by engineer and dropped when the
    active task changes. skipped_verification_run (199-228) returns passed:true when auto_run_tests=false,
    which the bar must treat as NOT passing. timed_out_verification_run (259-279) handles timeouts.
- file: src/team/merge/git_ops.rs
  why: commits_ahead_of_trunk (99) and ahead_count. The multi-repo variant is merge/completion/multi_repo.rs:291
    (unused here because multi-repo is excluded).
- file: src/team/worktree/ops.rs
  why: engineer_task_branch_name (65-70), engineer_base_branch_name (60-62), is_engineer_task_branch (78-82),
    branch_belongs_to_engineer (86-90), ahead_count (204), archive_branch / archive_branch_reusing (230-299),
    archive_branch_if_commits_ahead (308-339), abort_in_progress_git_operations (709-716).
- file: src/team/task_cmd.rs
  why: transition_task_with_attribution (27-50), move_task_validated (55-68),
    block_task_with_reason_and_attribution (119-134), reclaim_task_claim_with_attribution (296-320;
    writes todo without a transition check and does not clear assignee/branch/worktree_path),
    update_task_frontmatter (864-875; Mapping-based, keeps unknown keys).
- file: src/team/board.rs
  why: The WorkflowFrontmatter / read_workflow_metadata pattern (20-60, 117-123) for a new
    BenchRecoveryFrontmatter reader. Do NOT add fields to Task: there are about 40 struct literals.
- file: src/team/workflow.rs
  why: The transition table (89-120). Review->Todo is forbidden; InProgress->Todo and Blocked->Todo are allowed.
- file: src/team/daemon/automation/aging.rs
  why: maybe_auto_unblock_blocked_tasks (341-400) must skip tasks blocked at the reset cap.
- file: src/team/agent_options.rs
  why: all() (34-78) errors when effort is set on an unsupported backend (54-64). resolve(root, member)
    (80-98) is the single injection point for an effort override. validate_value (25-32).
- file: src/team/init.rs
  why: backend_supports_effort / canonical_backend (19-24) to decide skip-with-log.
- file: src/team/daemon/launcher.rs
  why: Effort is consumed at 680-711 (Claude --effort, Codex -c model_reasoning_effort) and recorded
    in .batty/launch-options/<member>.json (714-721).
- file: src/team/daemon/shim_spawn.rs
  why: BATTY_AGENT_EFFORT for Codex SDK (134-152).
- file: src/team/daemon/health/preflight.rs
  why: 113-125 report "launch settings missing or changed" when effort differs from config; make it
    override-aware.
- file: src/team/daemon.rs
  why: clear_active_task (1562) clears the zero-diff and narration counters and kills the
    verification job; reuse it on reset. set_active_task_for_test (1384).
- file: src/team/events.rs
  why: TeamEvent constructors use `{ ..Self::base("name") }`; task_escalated (415-422) is the example.
- file: src/team/daemon/telemetry.rs
  why: emit_event (40), record_task_escalated (500-526; 600s dedupe), record_orchestrator_action (969).
- file: src/team/delivery/routing.rs
  why: queue_daemon_message(recipient, body) (1009-1016) sends a user-role recipient to its channel.
    Human recipients are roles filtered by RoleType::User (see chat/bridge.rs:211-218).
- file: src/team/merge/completion/gates.rs
  why: block_escalated_task (385-418) is the blocked + blocked_on pattern; DiffEvidence::collect (44-76).
- file: src/team/daemon/poll.rs
  why: TeamDaemon::tick (258-467). Register a new recoverable step after poll_background_tests /
    process_merge_queue (332-338) and before reconcile_active_tasks / maybe_auto_dispatch (344-353).
- file: src/team/daemon/error_handling.rs
  why: run_recoverable_step / run_loop_step (23-38).
- file: src/team/daemon/automation/housekeeping.rs
  why: Throttle pattern at 294-299 (`if self.scheduler.last_X.elapsed() < N { return }`).
- file: src/team/daemon/scheduler.rs
  why: Add last_bench_recovery and last_full_bench_alert as Option<Instant> (8-62). Do NOT use
    suppress_recent_escalation (72-88) for the hourly alert: it prunes every key with the caller's window.
- file: src/team/config/types.rs
  why: WorkflowPolicy (254-352) and its Default (403-450), ZeroDiffRecovery (391-397), AutoMergePolicy (~666).
    The nested-config pattern to mirror is MainSmokePolicy (488-514): #[serde(default = "fn")] on each
    field plus a manual Default.
- file: src/team/config/tests_advanced.rs
  why: The defaults test (503-530); add bench_recovery defaults there.
- file: src/team/test_support.rs
  why: TestDaemonBuilder (548-689; zero_diff_recovery defaults to Restart at 566; .workflow_policy at 611),
    engineer_member / manager_member (460-495), init_git_repo (265), git_ok (222),
    write_owned_task_file[_with_context] (287-334), write_open_task_file / write_board_task_file (719-770).
- file: src/team/daemon/chat/telegram.rs
  why: telegram_merge_command_routes_through_merge_queue (1456-1512) is the test pattern for an
    async approval job feeding the merge queue.
- file: src/team/daemon/tests.rs
  why: Zero-diff tests (862-1070); the worktree setup is task_loop::prepare_engineer_assignment_worktree (870-877).
- docfile: docs/brainstorming/2026-11-07-bench-recovery-automation.md
  why: The source decisions, risks and backlog.
```

### Current Codebase tree (relevant part)

```bash
src/
  git.rs                          # crate::git::cmd (cherry-pick allowed)
  team/
    bench.rs                      # bench state file + lock
    board.rs                      # WorkflowFrontmatter readers
    task_cmd.rs                   # status moves, reclaim, frontmatter updates
    workflow.rs                   # task state machine
    agent_options.rs              # per-member model/effort resolution
    init.rs                       # backend_supports_effort
    events.rs                     # TeamEvent
    config/types.rs               # WorkflowPolicy and nested policies
    worktree/ops.rs               # branch naming, archive, ahead_count
    daemon.rs                     # TeamDaemon, clear_active_task
    daemon/
      poll.rs                     # tick(): step list
      scheduler.rs                # throttle timestamps
      background_tests.rs         # TestJob holders (chat_merges)
      merge_queue.rs              # MergeRequest, MergeOrigin, landing
      telemetry.rs                # emit_event, escalation, orchestrator log
      completion_checks.rs        # run_automatic_verification_tracked
      verification_tracking.rs    # zero-diff counters (in memory)
      chat/commands.rs            # chat merge: template for daemon approval
      dispatch/queue.rs           # candidate ranking, assignment
      health/poll_shim.rs         # zero-diff strike + park
      automation/aging.rs         # auto-unblock (must skip capped tasks)
      automation/housekeeping.rs  # throttle pattern
    merge/completion/{execute,gates,verify_job,decision}.rs
```

### Desired Codebase tree with files to be added

```bash
src/team/
  bench.rs                        # MODIFY: BenchSource, BenchEntry.source/task_id, bench_engineer_with()
  bench_recovery.rs               # CREATE: BenchRecoveryFrontmatter read/write, effort ladder,
                                  #         meets-bar evidence helpers (pure, unit-tested)
  daemon/
    bench_recovery.rs             # CREATE: impl TeamDaemon { maybe_run_bench_recovery, timed unbench,
                                  #         reset path, all-benched pass, approval job polling, hourly alert }
    health/poll_shim.rs           # MODIFY: wrong-branch rescue + strike warning + auto_park source
    dispatch/queue.rs             # MODIFY: avoid_engineers reorder, effort override on assignment
    merge_queue.rs                # MODIFY: MergeOrigin::BenchRecovery + landing/telemetry branches
    background_tests.rs           # MODIFY: bench_approvals job map
    scheduler.rs                  # MODIFY: last_bench_recovery, last_full_bench_alert
    poll.rs                       # MODIFY: register the recoverable step
    automation/aging.rs           # MODIFY: skip reset-capped tasks
    health/preflight.rs           # MODIFY: override-aware effort mismatch check
  agent_options.rs                # MODIFY: apply the task effort override in resolve()
  events.rs                       # MODIFY: new event constructors
  config/types.rs                 # MODIFY: BenchRecoveryConfig in WorkflowPolicy
  test_support.rs                 # MODIFY: builder default bench_recovery.enabled = false
docs/config-reference.md, docs/reference/agent-efficiency.md, docs/migrating-to-0.12.md, CHANGELOG.md
```

### Known Gotchas of our codebase & Library Quirks

```rust
// CRITICAL: with_bench_lock is a create_new file lock with a 2s timeout. Never run git,
//   send messages or write the board inside its closure. Re-read bench state right before
//   unbenching or approving: `batty bench` can change it at any time.
// CRITICAL: zero-diff strike counts, active_tasks and recent_dispatches are IN MEMORY.
//   Persist reset count, avoid list and effort override in task frontmatter.
// CRITICAL: the pinned `assignee:` frontmatter is a HARD dispatch filter (queue.rs:1745).
//   The reset must clear it, or the task can only go back to the same engineer.
// CRITICAL: Review -> Todo is not a legal transition (workflow.rs). Use
//   reclaim_task_claim_with_attribution, or go through Blocked/InProgress.
// CRITICAL: daemon auto-save commits ("wip: auto-save ... [batty]", daemon.rs:699,
//   preserve_with_commit) count as commits ahead. Exclude subjects containing "[batty]" from the
//   attribution count, or the bar passes on WIP.
// CRITICAL: skipped_verification_run returns passed:true when auto_run_tests=false. For this
//   feature that means "bar NOT met": route to reset, never approve.
// CRITICAL: run_automatic_verification_tracked checks claim and task branch first; run it on the
//   task branch BEFORE releasing the claim. It is blocking: wrap it in TestJob, never call it on
//   the poll thread.
// CRITICAL: do not reuse start_verification_job (engineer-keyed; dropped when the active task changes).
// CRITICAL: agent_options::all errors if effort is set on a backend without effort support. The
//   override must check init::backend_supports_effort first and skip with a log.
// CRITICAL: preflight flags an effort that differs from team.yaml as a launch mismatch. Teach it
//   about the active task's effort_override.
// CRITICAL: maybe_auto_unblock_blocked_tasks would undo a cap block when depends_on are done. Skip
//   tasks with bench_reset_capped: true.
// CRITICAL: scheduler.suppress_recent_escalation prunes all keys with the caller's window. Use a
//   dedicated Option<Instant> for the hourly alert.
// CRITICAL: strike tracking is off for non-git and multi-repo projects (poll_shim.rs:653), and
//   enqueue_reviewed_merge refuses multi-repo. The feature must no-op there.
// CRITICAL: TestDaemonBuilder uses WorkflowPolicy::default(). With bench_recovery on by default,
//   existing all-benched tests would change, so the builder sets enabled=false and new tests opt in.
// GOTCHA: bench::validate_engineer reads team.yaml from disk. Tests must write a team config
//   first (see write_bench_test_team_config).
// GOTCHA: archived reset branches must not be picked up by task_branch_candidates
//   (completion_checks.rs:285-321) as recovery branches. Use the preserved/* archive helpers,
//   whose names do not match the /<id> suffix.
// GOTCHA: there is no effort ladder in the code; the defaults are configurable. Verified
//   2026-11-07: Claude Code 2.1.302 `--effort` accepts low, medium, high, xhigh, max. The Codex
//   API (codex-cli 0.166.0) accepts none, minimal, low, medium, high, xhigh, max for
//   model_reasoning_effort; `none` is left off the ladder because it turns reasoning off.
//   Support can vary by model.
```

## Implementation Blueprint

### Data models and structure

```rust
// src/team/bench.rs
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchSource {
    #[default]
    Manual,   // existing entries and `batty bench`
    AutoPark, // zero-diff park
}

pub struct BenchEntry {
    pub timestamp: String,             // RFC 3339; used as benched_at for the cool-off
    pub reason: Option<String>,
    #[serde(default)]
    pub source: BenchSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<u32>,
}

pub fn bench_engineer_with(
    root: &Path, engineer: &str, reason: Option<&str>,
    source: BenchSource, task_id: Option<u32>,
) -> Result<BenchEntry>;
// bench_engineer(root, eng, reason) keeps its signature and calls bench_engineer_with(.., Manual, None)

// src/team/config/types.rs (mirror MainSmokePolicy)
pub struct BenchRecoveryConfig {
    pub enabled: bool,                        // true
    pub cooloff_secs: u64,                    // 1800
    pub max_auto_resets: u32,                 // 2
    pub auto_approve_when_all_benched: bool,  // true
    pub wrong_branch_rescue: bool,            // true
    pub strike_warning: bool,                 // true
    pub effort_ladder: EffortLadders,         // claude: [low, medium, high, xhigh, max]
                                              // codex:  [minimal, low, medium, high, xhigh, max]
}
// WorkflowPolicy gains: #[serde(default)] pub bench_recovery: BenchRecoveryConfig

// src/team/bench_recovery.rs (pure; mirrors board.rs WorkflowFrontmatter)
#[derive(Default, Deserialize)]
pub struct BenchRecoveryFrontmatter {
    #[serde(default)] pub bench_reset_count: u32,
    #[serde(default)] pub avoid_engineers: Vec<String>,
    #[serde(default)] pub effort_override: Option<String>,
    #[serde(default)] pub bench_reset_capped: bool,
}
pub fn read_bench_recovery(task_path: &Path) -> BenchRecoveryFrontmatter;
pub fn write_bench_recovery(task_path: &Path, fm: &BenchRecoveryFrontmatter) -> Result<()>; // via update_task_frontmatter

/// One step up the ladder from `current` (or from "medium" when the member has none).
/// Returns None if the backend has no ladder or `current` is not on it; stays at the top.
pub fn next_effort(ladder: &[String], current: Option<&str>) -> Option<String>;

pub struct BarEvidence { pub commits_ahead_of_trunk: u32, pub attributed_commits: u32 }
pub fn bar_commits_met(e: &BarEvidence) -> bool { e.commits_ahead_of_trunk > 0 && e.attributed_commits > 0 }
/// Attributed count = commits in eng-main/<eng>..<task branch> whose subject lacks "[batty]".
pub fn attributed_commit_count(worktree: &Path, base: &str, task_branch: &str) -> Result<u32>;
/// The bar is met only by a real passing run: skipped or timed-out runs are not passes.
pub fn verification_meets_bar(run: &VerificationRunResult, auto_run_tests: bool) -> bool;

// src/team/daemon/background_tests.rs
pub(crate) struct BenchApprovalJob {
    pub task_id: u32, pub engineer: String, pub kind: BenchApprovalKind, pub job: TestJob,
}
pub(crate) enum BenchApprovalKind { ApproveReview, RecoverParked }

// src/team/daemon/merge_queue.rs
pub enum MergeOrigin { /* existing */, BenchRecovery }
```

### List of tasks, in order

```yaml
Task 1 (bench provenance):
MODIFY src/team/bench.rs:
  - ADD BenchSource, plus source/task_id on BenchEntry with #[serde(default)].
  - ADD bench_engineer_with(); keep bench_engineer() as a Manual wrapper.
  - ADD pub fn bench_entries(root) -> Result<BTreeMap<String, BenchEntry>> for callers that need provenance.
  - UPDATE the struct literal at src/team/status/tests.rs:2375.
  - TESTS: old JSON without source loads as Manual; bench_engineer_with persists source/task_id;
    the concurrency test still passes.
MODIFY src/team/bench.rs format_benched_engineers_section: show "(auto, task #N)" for AutoPark.

Task 2 (config):
MODIFY src/team/config/types.rs:
  - ADD BenchRecoveryConfig + EffortLadders, mirroring MainSmokePolicy (488-514); add to WorkflowPolicy + Default.
MODIFY src/team/config/tests_advanced.rs: assert defaults (503-530 pattern).
MODIFY src/team/test_support.rs TestDaemonBuilder: workflow_policy.bench_recovery.enabled = false by default.

Task 3 (pure helpers):
CREATE src/team/bench_recovery.rs (+ `pub mod bench_recovery;` in src/team/mod.rs):
  - BenchRecoveryFrontmatter read/write (mirror board.rs read_workflow_metadata; write via task_cmd::update_task_frontmatter).
  - next_effort, attributed_commit_count (git rev-list with --format=%s; skip "[batty]"),
    bar_commits_met, verification_meets_bar.
  - TESTS: ladder step/top cap/unknown value/None base; attribution excludes [batty] commits (init_git_repo
    fixture); verification_meets_bar is false for skipped (auto_run_tests=false) and timed-out runs;
    frontmatter round-trip keeps unrelated keys.

Task 4 (auto_park source):
MODIFY src/team/daemon/health/poll_shim.rs maybe_recover_zero_diff_completion_loop (713-717):
  - bench_engineer_with(.., Some("completion attribution requires operator review"), AutoPark, Some(task_id)).
  - TEST: a Park-mode test (builder .workflow_policy with zero_diff_recovery Park) asserts an AutoPark entry
    with task_id after THRESHOLD completions.

Task 5 (strike warning):
MODIFY poll_shim.rs maybe_track_zero_diff_completion after note_zero_diff_completion (681):
  - if policy.bench_recovery.enabled && strike_warning && count == THRESHOLD - 1:
      queue_daemon_message(member, text) where the text is mode-dependent (Park mentions parking; Restart
      mentions a restart); emit completion_attribution_warning; record_orchestrator_action.
  - TESTS: a warning is sent at count 1 with THRESHOLD 2, the wording differs by mode, and nothing is sent when disabled.

Task 6 (wrong-branch rescue):
MODIFY poll_shim.rs maybe_track_zero_diff_completion before the strike (before 681):
  - when the attributed count is 0 and the rescue is enabled: candidate commits = the engineer's commits
    created after the task was assigned, on eng-main/<eng> or on stray branches where
    branch_belongs_to_engineer, excluding "[batty]" and commits already on the task branch.
  - checkout the task branch in the engineer worktree; `git cherry-pick <oldest..newest>`; on conflict
    abort_in_progress_git_operations, then the strike counts as before.
  - on success: re-count; emit completion_branch_rescued(commits); record_orchestrator_action; and if
    count > 0, skip the strike.
  - TESTS: a commit on eng-main is rescued and no strike is recorded; a conflict aborts and the strike is
    recorded; another engineer's branch is never touched.

Task 7 (effort override application):
MODIFY src/team/agent_options.rs resolve(): when the member's active task (read from the board) has
  effort_override and init::backend_supports_effort(backend), return the override as effort; otherwise
  log at debug and return the configured effort.
MODIFY src/team/daemon/dispatch/queue.rs process_dispatch_queue after active_tasks insert (1640):
  - if the task's effort_override differs from the member's recorded launch effort
    (.batty/launch-options/<member>.json), call handle_shim_cold_respawn(member, "bench_recovery_effort").
MODIFY completion/merge landing path: clear effort_override from task frontmatter when the task reaches done.
MODIFY src/team/daemon/health/preflight.rs 113-125: compare against the override-aware effort.
  - TESTS: resolve() returns the override for a Claude member and ignores it for an unsupported backend;
    preflight reports no mismatch under an override.

Task 8 (avoid list in dispatch):
MODIFY dispatch/queue.rs enqueue_dispatch_candidates after rank (1078-1084), mirroring the retry-owner
  reorder at 1085-1100: stable-move engineers in the task's avoid_engineers to the back.
  - TESTS: an avoided engineer is ranked last but is still chosen when it is the only idle engineer.

Task 9 (reset path):
CREATE src/team/daemon/bench_recovery.rs impl TeamDaemon:
  fn reset_parked_task(&mut self, engineer: &str, task_id: u32) -> Result<()>
    - fm = read_bench_recovery(task)
    - if fm.bench_reset_count >= cfg.max_auto_resets:
        block the task (block_task_with_reason_and_attribution, blocked_on = "bench recovery: reset cap reached"),
        set bench_reset_capped=true, record_task_escalated + queue_daemon_message to the human roles,
        emit task_bench_reset_capped, clear_active_task(engineer), unbench_engineer(engineer). Return.
    - archive the task branch (worktree/ops archive_branch_if_commits_ahead), clear_active_task(engineer),
      reclaim_task_claim_with_attribution(.., next_action="reset by bench recovery"),
      clear assignee/branch/worktree_path (update_task_frontmatter + set_optional_string),
      fm.avoid_engineers.push(engineer), fm.effort_override = next_effort(ladder, current or "medium"),
      fm.bench_reset_count += 1, write_bench_recovery, unbench_engineer(engineer),
      emit task_bench_reset, record_orchestrator_action.
MODIFY src/team/daemon/automation/aging.rs maybe_auto_unblock_blocked_tasks: skip bench_reset_capped tasks.
  - TESTS: reset 1 and 2 increment the count and set the avoid list and effort; reset 3 blocks, escalates
    and unbenches; auto-unblock leaves a capped task blocked; the pinned assignee is cleared.

Task 10 (approval job + merge origin):
MODIFY src/team/daemon/background_tests.rs: add bench_approvals: HashMap<u32, BenchApprovalJob>.
MODIFY src/team/daemon/merge_queue.rs:
  - MergeOrigin::BenchRecovery; in the landing branch (1474-1482) approve with reviewer
    "daemon (all engineers benched)" via StatusTransitionAttribution::daemon(..) (mirror approve_chat_merge
    2643-2677); telemetry branch (1498-1516).
CREATE in daemon/bench_recovery.rs:
  fn start_bench_approval_job(task, engineer, kind): mirror chat/commands.rs 222-283 (status check,
    multi-repo refusal, skip if already queued or job running), TestJob::spawn_with_timeout(
    run_automatic_verification_tracked(worktree, resolved test_command)).
  fn poll_bench_approval_jobs(): mirror finish_chat_merge 286-369. On finish:
    passed = verification_meets_bar(run, auto_run_tests) && bar_commits_met(evidence)
    ApproveReview: re-check all_engineers_benched() && task still in review && auto_merge.enabled
      -> enqueue_reviewed_merge(origin=BenchRecovery), emit task_auto_approved, record_orchestrator_action.
      Not passed -> reset_parked_task path (via Blocked to respect the state machine).
    RecoverParked: passed -> hand to the normal completion merge (enqueue_reviewed_merge, origin BenchRecovery),
      then unbench; not passed -> reset_parked_task.
  - TESTS (pattern: chat/telegram.rs 1456-1512): an approval enqueues with origin BenchRecovery and a
    review_disposition attributed to the daemon; nothing is enqueued when auto_merge disabled; nothing when
    an engineer is unbenched mid-job; a failed test run leads to the reset.

Task 11 (orchestration step):
CREATE in daemon/bench_recovery.rs:
  fn maybe_run_bench_recovery(&mut self) -> Result<()>
    - return unless cfg.enabled && is_git_repo && !is_multi_repo (except the alert)
    - throttle: scheduler.last_bench_recovery < 30s -> return
    - entries = bench_entries(root)  (lock released immediately)
    - all_benched = every dispatchable engineer (role not on_demand) is in entries
    - for each AutoPark entry: if all_benched || age(timestamp) >= cooloff_secs and no job is running:
        start_bench_approval_job(task_id, engineer, RecoverParked)
        (no task_id, or the task is no longer active -> unbench and emit bench_auto_unbenched)
    - if all_benched && cfg.auto_approve_when_all_benched && auto_merge.enabled:
        for each review task without a running job: start_bench_approval_job(.., ApproveReview)
    - if all_benched && every entry is Manual: hourly alert to the human roles (scheduler.last_full_bench_alert);
      emit team_fully_benched
    - poll_bench_approval_jobs()
MODIFY src/team/daemon/scheduler.rs: last_bench_recovery, last_full_bench_alert.
MODIFY src/team/daemon/poll.rs tick(): run_recoverable_step("bench_recovery", ..) after
  process_merge_queue (332-338), before maybe_auto_dispatch (344-353).
MODIFY src/team/events.rs: constructors for the new events.
  - TESTS: a manual bench is never auto-unbenched; an auto_park entry unbenches after the cool-off and not
    before; all_benched ignores on_demand engineers; the hourly alert fires at most once per hour; no-op in a
    non-git builder.

Task 12 (docs):
MODIFY docs/config-reference.md (bench_recovery field list, like main_smoke at 262-271),
  docs/reference/agent-efficiency.md (bench recovery paragraph), docs/migrating-to-0.12.md (on by default;
  opt out with workflow_policy.bench_recovery.enabled: false), CHANGELOG.md (0.12.0 Added + Changed).
RUN ./scripts/generate-docs.sh only if CLI help changes (it should not).
```

### Per-task pseudocode

```rust
// Task 6: rescue before the strike (poll_shim.rs, inside maybe_track_zero_diff_completion)
let attributed = bench_recovery::attributed_commit_count(&worktree, &base, &task_branch)?;
if attributed == 0 && policy.bench_recovery.enabled && policy.bench_recovery.wrong_branch_rescue {
    // PATTERN: branch_belongs_to_engineer (worktree/ops.rs:86-90) for the "own branches" filter
    // GOTCHA: only commits created after the task's assignment time; never other engineers' branches
    if let Some(commits) = find_rescuable_commits(&worktree, member, &task_branch, assigned_at)? {
        match cherry_pick(&worktree, &task_branch, &commits) {   // crate::git::cmd(..).args(["cherry-pick", ..])
            Ok(()) => { self.emit_event(TeamEvent::completion_branch_rescued(member, task_id, &commits)); }
            Err(_) => { abort_in_progress_git_operations(&worktree); }  // strike proceeds
        }
    }
}
let count = /* re-count as today */;
if count == 0 { let n = self.verification.note_zero_diff_completion(task_id); maybe_warn(n); }

// Task 11: the all-benched check
fn all_engineers_benched(&self, entries: &BTreeMap<String, BenchEntry>) -> bool {
    let dispatchable: Vec<_> = self.config.members.iter()
        .filter(|m| m.role_type == RoleType::Engineer)
        .filter(|m| !self.config.team_config.role_def(&m.role_name).is_some_and(|r| r.on_demand))
        .collect();
    !dispatchable.is_empty() && dispatchable.iter().all(|m| entries.contains_key(&m.name))
}
```

### Integration Points

```yaml
CONFIG:
  - add to: src/team/config/types.rs WorkflowPolicy
  - pattern: MainSmokePolicy nested struct with #[serde(default = "fn")] fields
  - default: enabled=true; the TestDaemonBuilder default is enabled=false

DAEMON LOOP:
  - add to: src/team/daemon/poll.rs tick()
  - pattern: run_recoverable_step + scheduler throttle (housekeeping.rs:294-299)

MERGE:
  - add: MergeOrigin::BenchRecovery (merge_queue.rs:85-95) with landing + telemetry branches
  - entry: merge::enqueue_reviewed_merge (execute.rs:108-151)

DISPATCH:
  - avoid_engineers reorder in enqueue_dispatch_candidates; effort override after assignment

STATE (persisted):
  - bench file: source, task_id
  - task frontmatter: bench_reset_count, avoid_engineers, effort_override, bench_reset_capped

NOTIFICATIONS:
  - human roles (RoleType::User) via queue_daemon_message; escalation via record_task_escalated;
    every action via record_orchestrator_action

DATABASE / API / FRONTEND: not applicable (CLI and daemon only).
```

## Validation Loop

### Level 1: Syntax & Style

```bash
export CARGO_TARGET_DIR=target   # or a shared target dir
cargo fmt -- --check             # run `cargo fmt` to fix
cargo clippy --all-targets --all-features -- -D warnings
# Expected: no errors. If there are errors, READ them and fix.
```

### Level 2: Unit tests

```bash
cargo test --lib bench                 # bench + bench_recovery modules
cargo test --lib zero_diff             # strike, warning, rescue, park
cargo test --lib dispatch              # avoid list, effort override
cargo test --lib merge_queue           # BenchRecovery origin
cargo test --lib config                # defaults
cargo test --lib                       # full lib suite (~4.1k tests)
```

### Level 3: Integration (tmux; private server only)

```bash
env -u TMUX TMUX_TMPDIR=$(mktemp -d) cargo test --lib --features integration
cargo test --test scenarios --features scenario-test
```

## Final validation Checklist

- [ ] All tests pass: `cargo test` (and `cargo test --lib --features integration` on a private tmux server)
- [ ] No linting errors: `make lint` (fmt check + clippy -D warnings, all features)
- [ ] No type errors: covered by `cargo clippy --all-targets --all-features`
- [ ] Build succeeds: `cargo build`
- [ ] Manual test: in a scratch repo, `batty init --template simple`, set `zero_diff_recovery: park`, bench all three engineers with `batty bench` (manual: expect only the hourly alert), then simulate an auto_park (a test fixture) and confirm the reset or approval in `batty status` and the orchestrator log.
- [ ] Error cases: cherry-pick conflict, missing task file, task already moved, auto_merge disabled, unsupported effort backend, bench lock timeout. Each logs and leaves state consistent.
- [ ] Logs are informative but not verbose. Every automated action has one orchestrator line and one event.
- [ ] Docs updated: config-reference, agent-efficiency, migrating-to-0.12, CHANGELOG.
- [ ] No new dependencies in Cargo.toml.

---

## Anti-Patterns to Avoid

- ❌ Don't add fields to `Task`/`Frontmatter` (about 40 struct literals); use a frontmatter reader like `WorkflowFrontmatter`.
- ❌ Don't call blocking verification on the poll thread; use `TestJob`.
- ❌ Don't reuse `start_verification_job` or `decide_merge_route` for approval.
- ❌ Don't touch `Manual` bench entries automatically.
- ❌ Don't hold the bench lock across git, messaging or board writes.
- ❌ Don't hardcode the effort ladder or the cool-off; read them from `BenchRecoveryConfig`.
- ❌ Don't use `suppress_recent_escalation` for the hourly alert.
- ❌ Don't let the reset-cap block be undone by auto-unblock.
- ❌ Don't count `[batty]` auto-save commits as attributed work.

## Confidence

**7/10** for a one-pass implementation. Patterns exist for every piece, and the risky integrations (approval via the chat-merge pattern, frontmatter metadata, dispatch reorder) are mapped to exact lines. The remaining risks:
- The wrong-branch rescue's commit selection, which has no existing helper.
- The effort override threading through launch and preflight.
- On-by-default interactions with existing tests.

Tasks 1–4 and 9–11 are the critical path. Ship the rescue (Task 6) and effort (Task 7) behind their config flags if time is short.
