# Fork comparison: newdave/batty vs battysh/batty

Date: 2026-10-27

Compared trees:

- **Upstream:** `battysh/batty` `main` at `5637f78` ("Fix lint failures blocking CI", 2026-08-18).
- **Fork:** `newdave/batty` `main` at `37cd391`. Commits landed after that snapshot are listed under [Not in this snapshot](#not-in-this-snapshot).

The fork is **90 commits ahead and 0 behind** upstream. Those commits are 67 changes plus 23 merge commits. Upstream is an ancestor of the fork, so the fork can still be rebased or upstreamed cleanly.

## Method

- Ran the same static-metrics script over `src/` in both trees. Production code is everything above the first `#[cfg(test)] mod`; dedicated test files count as test code.
- Ran `cargo test --lib` and `cargo clippy --all-targets` in both trees.
- Ran three read-only regression reviews, each comparing the two trees in one area: daemon and merge pipeline; git, worktree and shim; user-facing surface and integrations. Every reported issue was checked against both trees.
- Issues fixed come from the commit history and the code reviews that led to them.

## Statistics

### Change volume

| Range | Commits | Files | Insertions | Deletions |
|---|---|---|---|---|
| Fork work before 2026-10-27 review (`5637f78..f911ca7`) | 65 | 95 | 6,335 | 1,346 |
| Review and refactor session (`f911ca7..37cd391`) | 25 | 190 | 17,977 | 15,281 |
| **Total** | **90** | **219** | **24,102** | **16,417** |

Where the changes landed, by share of files changed:

| Path | Share |
|---|---|
| `src/team/` (excluding `daemon/`) | 37% |
| `src/team/daemon/` | 28% (of which `health/` is 6.4%) |
| `src/` top level | 11% |
| `src/team/templates/` | 8% |
| `docs/` | 6% |
| `src/shim/` | 6% |

Non-merge commits by theme:

| Theme | Commits |
|---|---|
| Reliability and data safety | 27 |
| Codex and agent backends | 12 |
| Trunk and merge pipeline | 8 |
| Refactor and structure | 6 |
| Docs, templates and roles | 3 |
| Performance and poll loop | 1 |
| Tests and CI | 1 |
| Other | 9 |

### Codebase shape

| Metric | Upstream | Fork | Change |
|---|---:|---:|---:|
| Rust files in `src/` | 198 | 224 | +26 |
| Total lines | 203,264 | 210,687 | +3.7% |
| Production lines | 98,899 | 101,931 | +3.1% |
| Test lines | 104,365 | 108,756 | +4.2% |
| `#[test]` functions | 3,975 | 4,080 | +105 |
| Feature-gated tests (`integration`, etc.) | 120 | 135 | +15 |
| Production functions | 3,447 | 3,555 | +108 |
| Functions over 100 lines | 95 | 100 | +5 |
| Functions over 200 lines | 29 | 31 | +2 |
| `TeamDaemon` fields | 67 | 27 | −60% |
| `impl TeamDaemon` blocks | 48 | 53 | +5 |

The fork grew by about 3% while deleting about 4,900 lines of dead code, so its new functionality (Codex backend, trunk configuration, the background job model, the git runner, `ChatPlatform`) is roughly 8k production lines. Test code grew faster than production code.

### Risk patterns in production code

| Pattern | Upstream | Fork | Change | Why it matters |
|---|---:|---:|---:|---|
| `Command::new("git")` | 65 | 1 | −98% | One runner now applies timeouts, env cleanup and kill handling |
| All `Command::new(` calls | 122 | 48 | −61% | |
| Bare `ureq::get/post/request` | 8 | 0 | −100% | No HTTP calls without timeouts |
| `Instant::now() - …` | 7 | 2 | −71% | Panics on macOS within 2h of boot (the remaining 2 are safe) |
| `#[path = …]` | 26 | 1 | −96% | Module ownership is visible again |
| `allow(dead_code)` | 105 | 62 | −41% | No file-wide allows remain |
| `.expect(` | 98 | 56 | −43% | Fewer panics hidden by `catch_unwind` |
| `.unwrap()` | 304 | 284 | −7% | |
| Plain `fs::write` | 102 | 79 | −23% | More writes go through `atomic_file` |
| `libc::kill` | 17 | 12 | −29% | All remaining calls go through pid-validating helpers |
| `let _ = …` | 258 | 250 | −3% | Mostly logging and best-effort cleanup |
| `thread::sleep` | 57 | 60 | +5% | The in-loop 30–60s sleep is gone; new sleeps are in background workers and kill-grace loops |
| `unsafe {` | 36 | 37 | +1 | Signal and process-group calls |

### Tests

| | Upstream `5637f78` | Fork `37cd391` |
|---|---|---|
| `cargo test --lib` | 3,836 passed, **1 failed**, 119 ignored | 3,930 passed, 0 failed, 134 ignored |
| Other test targets | — | 149 + 2 + 7 passed |
| `cargo clippy --all-targets` warnings | 0 | 0 |
| `cargo build --tests --all-features` | — | compiles |

- The upstream failure (`daemon_mgmt::graceful_daemon_shutdown_times_out_before_force_kill_fallback`) is timing-dependent. The fork fixed it in `f911ca7`.
- Suite wall time was not comparable, because the machine was running other builds at the same time.
- In the fork, live-tmux tests are behind `integration`, as CLAUDE.md requires, and environment-mutating tests share one lock. In recent runs two tests still flaked under heavy load: `poll_shim::check_working_state_timeouts_restarts_stale_claude_management_agents` and `test_job::kill_terminates_tracked_command_process_group`.

### Largest functions

| Upstream | Lines | Fork | Lines |
|---|---:|---|---:|
| `main` | 1,321 | `main` | 1,353 |
| `handle_engineer_completion` | 1,021 | `shim::runtime::run` | 845 |
| `shim::runtime::run` | 844 | `finish_engineer_completion` | 837 |
| `run_kiro_acp` | 743 | `run_kiro_acp` | 723 |
| `run_sdk` | 664 | `handle_shim_event` | 698 |
| `handle_shim_event` | 641 | `run_sdk` | 667 |

After this snapshot, `handle_shim_event` became a ~55-line dispatcher (`8e40f5b`). Splitting `main` and `finish_engineer_completion` is in progress (see below).

## Issues fixed

### Crashes, hangs and blocked daemon loop
- The daemon panicked at startup when uptime was under 2h (`Instant` underflow on macOS).
- A stalled agent made the daemon loop sleep for 30–60s, freezing the whole team.
- Verification, the main smoke check, chat merges and post-merge verification ran full test commands inside the poll loop (4+ minute stalls). They now run on background jobs with process-group kill and survive restarts.
- Telegram and Discord HTTP calls had no timeouts. A failed Telegram send was retried every poll for every message: about 3,500 requests an hour, each blocking about 1.5s.
- `remove_member` busy-waited inside the loop. Per-engineer git checks and the board parse ran about 15 times per tick (the board is now parsed once).
- The tmux wrapper could deadlock on large output. The Grafana `curl` calls had no timeouts.

### Data loss and integrity
- Merge-queue requests were dropped on any error or panic. Queued merges, dispatches and messages were lost when processing failed.
- Auto-merge checks failed open on git errors, so a broken diff looked like a small clean change.
- Archive failures were ignored before `checkout -B`. Force resets ran with no saved copy. Dispatch reset lanes without preserving their commits.
- An isolated merge moved the checked-out trunk ref and left the root index behind, so a `commit -a` would revert the merge.
- Rebase auto-resolve could stage conflict markers and silently drop engineers' template edits.
- Multi-repo merges were non-atomic.
- Worktrees were left on other members' branches after reassignment.
- Board and activity-log writes raced kanban-md. State, registry and `.env` writes were not atomic. The merge lock could be reclaimed by two waiters at once.
- Shim message frames could be corrupted by partial writes and partial reads.
- The legacy command queue lost commands. Only the last of several in-flight messages was replayed after a shim died.

### Correctness
- Status moves bypassed the task state machine. Completion handling tried illegal Done→Blocked and Backlog→Review moves. Verification escalations repeated every few minutes.
- The openclaw Telegram provider was called with the wrong flag (`--to` instead of `--target`).
- Concurrent Codex writers on the same thread caused thread-store conflicts. The sdk and kiro runtimes leaked process trees.
- Stale-member cleanup, merge-conflict recovery and malformed task files caused warn-every-tick loops.
- Five daemon events never reached SQLite or Grafana.

### Maintainability
- One git runner (`src/git.rs`) and one set of worktree operations (`src/worktree/ops.rs`) replace five runners and many duplicated helpers.
- `TeamDaemon` is split into 8 owned state groups. The `#[path]` includes are now a real module tree. The chat bridges share one `ChatPlatform` flow.
- Removed about 4,900 lines of dead or unreferenced code. File-wide `allow(dead_code)` is gone.
- Tests use one environment lock, shared fixtures and builders, and gated live-tmux tests.

## Issues caused (regressions and risks)

Ranked by severity. Every item was checked against both trees. "Origin" says whether the issue comes from fork work before the review session or from the session itself.

| # | Severity | Issue | Evidence (fork) | Origin |
|---|---|---|---|---|
| 1 | High | With a non-default `trunk_branch` in isolated-merge mode, post-merge verification and revert read the root HEAD instead of `refs/heads/<trunk>`. A failed verify reports "reverted" while the bad merge stays on trunk. | `daemon/merge_queue.rs` `git_head(project_root)` around 1065/1076/1635; `merge/operations.rs:254` | trunk_branch + async verify |
| 2 | High | `batty config --json` changed shape (`{config_path, members, config}`), and a missing config now exits non-zero. Scripts reading `.team`, `.roles`, `.board.*` and similar break. | `src/main.rs` ~765, `config/mod.rs` | session |
| 3 | High | The published lib crate lost `pub mod events`, `log` and `config` without a version bump (0.11.63). | `src/lib.rs` | session |
| 4 | High | The default `batty init` template and prompts now turn on `auto_merge`, `auto_dispatch` and a different role set, and drop the orchestrator pane. Unattended merge-to-trunk is on by default. | `team/templates/team_simple.yaml`, interactive init defaults | pre-session |
| 5 | Medium | A merge finishing after a restart or hot reload can clear the engineer's next task. `complete_landed_merge` calls `clear_active_task` without checking the task id. | `merge_queue.rs` 964/1149/1213 | session (async verify) |
| 6 | Medium | The merge lock is released before post-merge verification. Chat and direct merges can land while verification runs, causing flaky failures and manual reverts. | `merge_queue.rs:1077`, `chat/commands.rs`, `completion.rs:1598` | session |
| 7 | Medium | The automatic revert runs `git reset --hard` on the root without checking for uncommitted changes, possibly hours later after a restart. | `merge_queue.rs:1656` (`reset_main_to`) | session |
| 8 | Medium | The flock `MergeLock` is incompatible with older binaries. The file is never deleted, so old binaries time out and refuse stale restarts. An empty file from an old holder reads as unheld. | `merge/lock.rs:57, 166` | session |
| 9 | Medium | Health-check recovery resets fail forever in sub-repos whose default branch differs from the configured trunk: the archive step uses the raw trunk while the checkout uses the per-repo effective trunk. | `worktree/ops.rs:316-329` vs `:930` | session (ops consolidation) |
| 10 | Medium | Every git call now has a hard timeout ending in SIGKILL (120s default). `reset`, `stash`, `add`, `status` and `clean` count as short. A killed command leaves `index.lock` behind, and timeouts are retried as transient. | `src/git.rs:29-58, 530-563`, `git_cmd.rs:47` | session |
| 11 | Medium | `docs/reference/config.md` was deleted but is still referenced by `mkdocs.yml` nav, the README, docs and the CI docs-drift check. | `mkdocs.yml:24`, `.github/workflows/ci.yml:307` | session |
| 12 | Medium | Telegram and Discord drop commands sent while the daemon was down, with no notice to the sender. | `telegram.rs` (offset=-1), `discord.rs` (limit=1) | pre-session |
| 13 | Medium | Telemetry `events` and `review_queue_metrics` are pruned after 90 days. This is not configurable or documented, and the first prune needs 24h of continuous uptime. | `telemetry_db.rs` `run_retention`, `daemon/scheduler.rs:53` | session |
| 14 | Medium | The openclaw arguments changed to `--channel telegram --target`, so older openclaw releases that only accept `--to` break. `validate` now errors on unknown channels and empty targets. | `comms.rs` `provider_send_args`, `config/channels.rs` | session |
| 15 | Low-med | Git runs in its own process group. Hooks or ssh signing that read `/dev/tty` hang until the timeout, and Ctrl-C no longer reaches git in interactive `batty merge`. | `src/git.rs:350` | session |
| 16 | Low-med | Lock errors on a filesystem without `flock` support make the revert retry every tick, with no escalation. | `merge_queue.rs:738-748` | session |
| 17 | Low-med | Removing a member and re-adding the same name within the shutdown timeout kills the new member's shim, pane and worktree. | `daemon/reconcile.rs:216-265` | session |
| 18 | Low | `loop_step_error` events are throttled exponentially, so Grafana panels under-count persistent failures. | `daemon/telemetry.rs:328` | session |
| 19 | Low | `TestJob` has no timeout. A hung post-merge test leaves the queue stuck in "verifying" with no escalation. | `team/test_job.rs` | session |
| 20 | Low | `merge --ff-only` deferral into a persistently dirty root never escalates. | `merge/operations.rs:395` | session |
| 21 | Low | A completion that arrives without an id pops the oldest replayable message, which can cause duplicate or missed replays. | `daemon/agent_handle.rs:151, 263` | session |
| 22 | Low | Legacy queue sends to a user role with no channel are retried every tick forever. A crash mid-batch re-delivers the batch. | `delivery/routing.rs:1213`, `legacy_command_queue.rs:144` | session |
| 23 | Low | Frontmatter parsing is stricter: the closing line must be exactly `---`, so files that used to parse can now be skipped. | `task/frontmatter.rs` | session |
| 24 | Low | `init --agent` and `--name` round-trip the template through serde_yaml, losing comments and layout, and backend changes strip model and effort pins. | `team/init.rs` | session |
| 25 | Low | The Telegram chunk-progress key is a hash of (chat, text), so an identical later message can skip chunks after a partial failure. | `team/telegram.rs` | session |
| 26 | Low | Fallback status moves to done are refused unless the task is in review (reported to the manager, not forced). | `daemon/dispatch/mod.rs` `validated_move` | session |
| 27 | Low | Two tests remain flaky under heavy load (see Tests). | — | pre-existing and session |

None found:
- **Persistence:** old state files load (new fields have serde defaults).
- **Stale results:** stale verification results are discarded.
- **Events:** all upstream event names are still emitted.
- **Shim framing:** EOF handling is unchanged.
- **Deleted code:** none of it had remaining references in `src/`, `tests/` or `docs/`.

## Future work

### A. Rectify introduced issues (priority order)

1. **Make the trunk-aware merge pipeline correct (#1, #5, #6, #7).** Effort: M.
   - Read and revert `refs/heads/<trunk>` with `update-ref <pre> <post>`.
   - Verify in a trunk worktree.
   - Keep the merge lock in `InFlightMerge`, and make chat and direct merges defer while a merge is verifying.
   - Clear an engineer's task only if it matches the merged task.
   - Refuse to reset a dirty root (or use `reset --keep`).
   - Add an end-to-end test with `trunk_branch` and `post_merge_verify` both on.
2. **Restore compatibility promises (#2, #3, #4, #14).** Effort: S.
   - `batty config --json`: keep the legacy top-level keys next to `config`, or add `schema_version`.
   - Bump the minor version, or add re-export shims for the removed lib modules.
   - Make auto-merge opt-in in the default template again.
   - Document the minimum openclaw version.
   - Add a `CHANGELOG` entry for every behaviour change listed above.
3. **Make git failures recoverable (#9, #10, #15, #20).** Effort: S–M.
   - Use the per-repo effective trunk in `archive_checked_out_work`.
   - Move `reset`, `stash`, `add`, `status` and `clean` to the long timeout.
   - On timeout, send SIGTERM, wait a grace period, then SIGKILL.
   - Clean up `index.lock` older than the kill.
   - Treat timeouts as non-transient.
   - Skip the separate process group when attached to a TTY.
   - Escalate after N dirty-root deferrals.
4. **Lock compatibility and robustness (#8, #16).** Effort: S.
   - Unlink the lock file on drop while still holding it.
   - Fall back to exclusive-create when `flock` is unsupported.
   - Cap revert retries and escalate.
5. **Telemetry and docs (#11, #13, #18).** Effort: S.
   - Fix the dangling `config.md` references and the CI check.
   - Add `telemetry.retention_days` (0 disables), document it, and run the first pass at startup.
   - Emit a periodic aggregate for throttled loop errors.
6. **Lifecycle edge cases (#12, #17, #19, #21, #22, #25).** Effort: S each.
   - Reply "ignored N commands sent while offline".
   - Cancel pending removal on re-add.
   - Add a `TestJob` wall-clock timeout that counts as a failure.
   - Retire in-flight messages by id only.
   - Add a TTL to the legacy queue.
   - Clear chunk progress on a full send.
7. **Test stability (#27).** Effort: S.
   - Make the two load-sensitive tests wait on conditions instead of fixed deadlines.
   - Add a CI job that runs the suite twice to catch new flakes.

### B. Areas of focus beyond the fixes

- **Upstream relationship.** The fork is 90 commits ahead and nothing behind. Decide whether to upstream. If yes, send the reliability and data-safety commits first: they are self-contained and match upstream's priorities. Hold back the template-default changes and large refactors until upstream agrees on direction.
- **End-to-end confidence.** The biggest gap is behaviour across restarts, hot reloads and real git repositories. Build a scenario harness (`tests/scenarios/` already exists) for: merge with post-merge verify failure, restart mid-verify, member remove and re-add, multi-repo merge, and a non-`main` trunk. Run the `integration` and `shim-integration` features in CI with a tmux server.
- **Remaining structural debt.**
  - `shim::runtime::run` (845 lines), `run_kiro_acp` (723) and `run_sdk` (667) share a lifecycle. The new `shim/child.rs` is the start of a common `StructuredRuntime`.
  - `automation.rs` (3.4k lines) and `status.rs` (3.3k) should be split by concern.
  - About 62 item-level dead-code allows remain.
  - The three modules named `verification` should be renamed.
- **Observability of the new async machinery.** Expose in-flight merges, background tests, deferred retries and lock holders in `batty status` and Grafana, with age and stuck-state alerts, so the new non-blocking flows are visible when they stall.
- **Configuration hygiene.** Remove the seven deprecated config keys in the next minor release. Generate config docs from the serde types again, replacing the deleted `config.md` generator.
- **Security.** `batty telegram` writes the bot token into `team.yaml`. Move it to `.env`, as Discord already does. Audit for other secrets written to files that may be tracked in git.

## Not in this snapshot

- `8e40f5b` — `handle_shim_event` split per event, and dispatch, pane labels and reconcile reading the board snapshot. Committed after `37cd391`; 3,931 tests pass.
- `2af29b2` — `merge/completion.rs` split into a `completion/` module. Chat and single-repo direct merges now go through the queue, and the engineer's task is cleared only if it matches the merge. This partly fixes #5 and #6. It also surfaced #28 (multi-repo scope-fence crash, pre-existing) and #29 (deferred multi-repo merges kept only in memory). See [fork-fix-plan.md](fork-fix-plan.md).
- Not merged: the `main()` split into `src/commands/`, held pending a decision on whether it is worth the upstream-merge churn.
