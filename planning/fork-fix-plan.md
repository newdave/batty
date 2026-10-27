# Fork fix plan

Date: 2026-10-27

> **Status (second comparison run, fork `96d2dc8`):** all tasks have landed: streams A–F, T-G1 and T-G2. 24 of the 29 issues are fixed and 5 are partly fixed. The second run found new issues N1–N17. See [fork-comparison.md](fork-comparison.md#new-issues-found-in-this-run) for their status and the next steps.

This plan fixes the 27 issues in [fork-comparison.md](fork-comparison.md#issues-caused-regressions-and-risks). The `#n` references are that document's issue numbers.

## How to use this plan

- **Streams** group tasks that touch the same files. Different streams can run in parallel. Tasks within a stream run in order.
- **Effort:** S is under half a day, M is about one day, L is more than one day.
- **Every task must:**
  - add unit tests (no tmux) that fail before the fix and pass after;
  - keep `cargo clippy --all-targets`, `cargo build --tests --all-features` and the full `cargo test` green;
  - add a `CHANGELOG.md` line under "Unreleased" for any user-visible behaviour change.
- **Base:** start from `2af29b2` or later. That commit landed the `merge/completion/` split and moved direct and chat merges into the queue. That already covers T-A2 and most of T-A3; both are re-scoped below.
- **New issues** found after fork-comparison.md was written:
  - **#28 (High):** a multi-repo completion crashes in the scope fence when the task has an owner, because it runs `git diff main..HEAD` on the multi-repo root, which isn't a git repo.
  - **#29 (Medium):** deferred multi-repo direct merges are held in memory only and are lost on restart.
  - Both are added as tasks T-A7 and T-A8.

## Summary

| Stream | Theme | Tasks | Issues | Effort |
|---|---|---|---|---|
| A | Merge pipeline correctness | T-A1 to T-A8 | #1 #5 #6 #7 #8 #16 #20 #28 #29 | M+S+S+S+M+S+S+S |
| B | Git runner robustness | T-B1 to T-B3 | #9 #10 #15 | S+M+S |
| C | Compatibility and docs | T-C1 to T-C5 | #2 #3 #4 #11 #14 | S each |
| D | Telemetry and chat | T-D1 to T-D4 | #12 #13 #18 #25 | S each |
| E | Lifecycle edge cases | T-E1 to T-E4 | #17 #19 #21 #22 | S each |
| F | Board and init | T-F1 to T-F3 | #23 #24 #26 | S+M+S |
| G | Test stability and release | T-G1 to T-G2 | #27, plus release notes | S+S |

Suggested waves:
1. A1, B1, C1–C5, D1–D4, E1–E4, F1–F3 in parallel, one agent per stream.
2. A2–A6 and B2–B3.
3. G1–G2.

---

## Stream A: merge pipeline correctness

Files: `src/team/daemon/merge_queue.rs`, `src/team/merge/lock.rs`, `src/team/merge/operations.rs`, `src/team/daemon/chat/commands.rs`, `src/team/merge/completion/*`.

### T-A1: Verify and revert the trunk ref, not the root HEAD (#1) — M
- **Problem:** `pre_merge_head` and `post_merge_head` come from `git_head(project_root)`. In isolated-merge mode with a non-default `trunk_branch`, only `refs/heads/<trunk>` moves. Verification then runs on a checkout without the merge, and a failed verification reports `AlreadyReverted` while the bad merge stays on trunk.
- **Change:**
  - Record `pre` and `post` from `git rev-parse refs/heads/<trunk>`.
  - Run post-merge verification in a dedicated worktree checked out at the trunk tip, e.g. `.batty/verify-worktree`, created and refreshed with `worktree add --detach`. Never run it in the root unless the root is on trunk.
  - Revert with `git update-ref refs/heads/<trunk> <pre> <post>`. This is atomic and only applies if the ref still points at `post`. Then refresh the root only if its HEAD is that trunk.
  - `plan_post_merge_revert` compares the trunk ref, not HEAD.
- **Tests:**
  - With `trunk_branch=develop` and the root on another branch, a failing post-merge verification moves `develop` back to `pre`.
  - The same setup with a passing verification leaves `develop` at `post`.
  - If the trunk moved after the merge, the revert is skipped and escalated.
  - Remove `should_post_merge_verify: false` from the integration-branch test.
- **Acceptance:** with any `trunk_branch`, after a failed verification `refs/heads/<trunk>` equals `pre`, or an escalation is emitted. No "reverted" notice is sent unless the ref actually moved.

### T-A2: Only clear the engineer's task that was merged (#5) — S, mostly done in 2af29b2
- **Done:** after a queued merge finishes, the engineer's assignment is cleared only if it is still that task.
- **Remaining:**
  - Apply the same guard to the conflict and failure paths, and to `set_member_idle`, in `merge_queue.rs`. Check every `clear_active_task` call reached from a merge outcome.
  - Add a test: reassign to task B, then let task A's in-flight merge fail. B stays active and the engineer stays working.
- **Acceptance:** finishing merge A, whatever the outcome, never changes the engineer's active task or idle state when it isn't A.

### T-A3: Serialize every merge path against in-flight verification (#6) — S, mostly done in 2af29b2
- **Done:**
  - Chat merges and single-repo direct completions go through the queue.
  - Multi-repo direct merges use `try_acquire` and defer while a queue merge is verifying or the smoke check runs.
  - `batty merge` fails fast while a merge is in flight.
- **Remaining:**
  - Store the `MergeLock` guard in `InFlightMerge` until finalize or revert, so other processes, such as an older binary or `batty merge` from another shell, are excluded through the lock itself, not only through daemon state.
  - Re-acquire the lock when an in-flight merge is restored.
- **Tests:** after a restore, the in-flight merge holds the lock, and `MergeLock::try_acquire` from a second handle fails until it finishes.
- **Acceptance:** at most one change lands on trunk between a merge and the end of its verification, including from processes outside the daemon.

### T-A4: Never hard-reset a dirty root during the automatic revert (#7) — S
- **Change:**
  - Before `reset_main_to`, run `git status --porcelain` on the root, excluding Batty runtime paths.
  - If source changes are present, don't reset. Escalate to the manager and operator, listing the paths and naming `pre`, and keep the revert pending.
  - Otherwise use `git reset --keep <pre>` instead of `--hard`.
  - With T-A1, the revert normally goes through `update-ref`, so this applies to the root refresh step.
- **Tests:**
  - A dirty root during the revert leaves files untouched and emits an escalation.
  - A clean root is reset.
  - `reset --keep` refusing is surfaced as an escalation.
- **Acceptance:** an automatic revert never discards uncommitted root changes.

### T-A5: Make the merge lock compatible and robust (#8, #16) — M
- **Change:**
  - On drop, while still holding the `flock`, truncate the file and then `unlink` it. Old binaries that test `exists()` or use `create_new` then see a free lock.
  - On acquire, open with `create(true)`.
  - Treat a file that exists, isn't locked, is empty, and is newer than N seconds as possibly held by an old binary: write a PID marker on acquire, and in `merge_lock_is_held` treat a live foreign PID as held.
  - If `flock` returns ENOLCK or ENOTSUP, fall back to the old exclusive-create lock with PID reclaim.
  - Count consecutive revert lock failures. Escalate after 5, then retry every 5 minutes instead of every tick.
- **Tests:**
  - The file is removed after drop.
  - A lock file created by the old scheme with a live PID blocks acquisition.
  - The fallback path is used when `flock` is unsupported (inject the error).
  - Revert failures escalate once after 5 attempts.
- **Acceptance:** old and new binaries never both think they hold the lock, an old binary isn't stuck after an upgrade, and lock failures are visible.

### T-A6: Escalate persistent dirty-root merge deferrals (#20) — S
- **Change:**
  - When `merge --ff-only` refuses because of overlapping dirty or untracked files, record the deferral count per request.
  - After 3 deferrals, or 30 minutes, escalate with the overlapping paths from git's stderr.
  - Keep retrying on the normal backoff.
- **Tests:**
  - The third deferral emits one escalation that lists the paths.
  - Cleaning the root lets the next retry land.
- **Acceptance:** a merge blocked by a dirty root always produces an operator-visible notice within 30 minutes.

### T-A7: Fix the multi-repo scope fence crash (#28) — S
- **Problem:** the completion scope fence (`merge/completion/gates.rs`) runs `git diff <trunk>..HEAD` on the worktree root. In multi-repo projects that root isn't a git repo, so any completion of a claimed task errors. Claimed is the normal case.
- **Change:** in multi-repo mode, compute the diff per sub-repo (reuse the multi-repo helpers in `merge/completion/multi_repo.rs`), prefix the paths with the sub-repo name, and run the fence over the combined path list.
- **Tests:**
  - A claimed multi-repo task with in-scope changes in two sub-repos passes the fence.
  - Out-of-scope changes in one sub-repo are reverted there only.
  - Drop the "unclaimed task" workaround from the existing multi-repo tests.
- **Acceptance:** multi-repo completions of claimed tasks run the scope fence without error.

### T-A8: Persist deferred multi-repo direct merges (#29) — S
- **Problem:** deferred multi-repo direct merges (`poll_deferred_direct_merges`) are held in memory, so a restart leaves the task in review with nothing retrying it.
- **Change:**
  - Persist the deferred set (engineer, task id, deferred-since) in `PersistedDaemonState`, with a serde default.
  - Restore it on resume, dropping entries whose task is no longer in review or claimed by that engineer.
  - Escalate a deferral older than 30 minutes, matching T-A6.
- **Tests:**
  - A deferred merge survives a persist and restore round trip, then lands when the lock frees.
  - A stale entry for a reassigned task is dropped on restore.
- **Acceptance:** no deferred merge is lost across a restart or hot reload.

---

## Stream B: git runner robustness

Files: `src/git.rs`, `src/worktree/ops.rs`, `src/team/git_cmd.rs`, `src/team/errors.rs`.

### T-B1: Use the per-repo effective trunk when archiving before a reset (#9) — S
- **Problem:** `archive_checked_out_work` (`worktree/ops.rs:316-329`) uses the configured trunk, while the checkout uses `effective_trunk_branch_for_repo`. Sub-repos whose default branch is `master` or `mainline` fail with "unknown revision" forever.
- **Change:**
  - Resolve the effective trunk inside `archive_checked_out_work`.
  - If the trunk ref is missing, or HEAD is detached with no comparable base, archive the current HEAD unconditionally (name it with the `preserved/` scheme) rather than failing.
- **Tests:**
  - A multi-repo setup with a `mainline` sub-repo: health-check recovery resets succeed and archive the branch.
  - A detached-HEAD sub-repo is archived and reset.
- **Acceptance:** recovery never fails solely because the configured trunk is missing in a sub-repo.

### T-B2: Timeout classes, graceful kill, lock cleanup (#10) — M
- **Change:**
  - Classify `reset`, `stash`, `add`, `status`, `clean`, `checkout`, `diff` and `merge-tree` as long commands. Keep the short class for read-only plumbing (`rev-parse`, `rev-list`, `for-each-ref`, `config`).
  - On timeout, send SIGTERM to the group, wait 5s, then SIGKILL.
  - After a killed command, if `.git/index.lock` (resolved via `git rev-parse --git-path index.lock`) is older than the kill time and no git process remains in the group, remove it and log a warning.
  - In `git_cmd`, map `GitError::Timeout` to a permanent error so `retry_git` doesn't block for about 6 minutes.
  - Document `BATTY_GIT_TIMEOUT_SECS` and `BATTY_GIT_LONG_TIMEOUT_SECS` in `docs/config-reference.md`.
- **Tests:**
  - The command class mapping.
  - A process that ignores SIGTERM gets SIGKILL after the grace period.
  - A stale `index.lock` created by a killed fake git is removed. A fresh one owned by a live process is not.
  - A timeout isn't retried.
- **Acceptance:** no git operation that legitimately takes longer than 120s is killed with default settings, and a timeout never leaves a stale `index.lock` behind.

### T-B3: Let interactive git reach the terminal (#15) — S
- **Change:**
  - When stdin of the batty process is a TTY (`isatty(0)`), and for commands that may run hooks or signing (`commit`, `merge`, `rebase`, `tag`, `push`), don't create a separate process group, and let SIGINT reach git.
  - The daemon never has a TTY, so its behaviour is unchanged.
- **Tests:** the process-group decision as a pure function of (`is_tty`, `subcommand`).
- **Acceptance:** `batty merge` from a terminal can run hooks that read `/dev/tty`, and Ctrl-C stops git.

---

## Stream C: compatibility and docs

Files: `src/commands/*` or `src/main.rs`, `src/team/config/mod.rs`, `src/lib.rs`, `Cargo.toml`, `src/team/templates/*`, `src/team/init.rs`, `src/team/comms.rs`, `mkdocs.yml`, `README.md`, `docs/**`, `.github/workflows/ci.yml`.

### T-C1: `batty config --json` compatibility (#2) — S
- **Change:**
  - Emit the legacy top-level keys again (`team`, `roles` as a count, `board{}`, `standup{}`, `automation{}`, `workflow{mode, orchestrator_pane}`) with their old values, plus `"schema_version": 2` and the full `config` object.
  - A missing config keeps the new behaviour (error and non-zero exit). Document this in the changelog.
- **Tests:** a snapshot test comparing the legacy keys against a fixture captured from upstream `5637f78`'s output, plus `schema_version`.
- **Acceptance:** scripts written against upstream's JSON keep working.

### T-C2: Library API compatibility (#3) — S
- **Change:** bump `Cargo.toml` to `0.12.0`, because modules were removed from a published crate. Add a changelog "Removed" section listing `batty_cli::{events, log, config}` and the move of `config::Policy` to `task::Policy`. Don't restore the dead modules.
- **Acceptance:** the version and changelog reflect the breaking API change before any publish. `cargo package --list` succeeds.

### T-C3: Make auto-merge opt-in in the default template (#4) — S
- **Change:**
  - In `team_simple.yaml` and the interactive `batty init` prompts, set `auto_merge.enabled: false` and default the prompt answer to "no".
  - Keep the other new defaults, but list every default change in the changelog: `auto_dispatch`, `orchestrator_pane`, role set, engineer count, standup, nudge and stall timings.
- **Tests:** template parse test asserting `auto_merge.enabled == false`. Init prompt default test.
- **Acceptance:** a fresh `batty init` never merges to trunk without explicit opt-in.

### T-C4: openclaw argument compatibility (#14) — S
- **Change:**
  - In `TelegramChannel::send_via_provider`, if the provider exits non-zero with stderr matching `unknown option '--channel'|'--target'`, retry once with the legacy `--to <target> --message <msg>` form, and remember the form that works for this channel instance.
  - Document the minimum openclaw version that supports `--target`.
  - `batty validate` warns, but doesn't error, when the provider binary is older than that version, if `<provider> --version` is parseable.
- **Tests:**
  - A fake provider rejecting `--target` gets the legacy form and succeeds.
  - A fake provider accepting `--target` gets the new form only.
- **Acceptance:** both old and new openclaw releases deliver messages.

### T-C5: Fix docs links to the deleted config reference (#11) — S
- **Change:**
  - Remove `docs/reference/config.md` from the `mkdocs.yml` nav and from the CI `git diff --exit-code` check.
  - Point the `README.md`, `docs/config-reference.md`, `docs/contributing.md` and `docs/reference/modules.md` links at `docs/config-reference.md`.
  - Run `mkdocs build --strict` in CI if it isn't already run.
- **Acceptance:** `mkdocs build --strict` passes, and no repo file references `docs/reference/config.md`.

---

## Stream D: telemetry and chat

Files: `src/team/telemetry_db.rs`, `src/team/daemon/telemetry.rs`, `src/team/daemon/scheduler.rs`, `src/team/config/types.rs`, `src/team/telegram.rs`, `src/team/discord.rs`, `src/team/daemon/chat/*`.

### T-D1: Configurable telemetry retention (#13) — S
- **Change:**
  - Add `telemetry.retention_days` (default 90; 0 disables) to the config types and docs.
  - Run the first retention pass shortly after startup (initialise `last_telemetry_retention` to `None`), then every 24h.
  - Log the number of rows deleted.
- **Tests:**
  - `retention_days: 0` deletes nothing.
  - A custom value prunes at that age.
  - The first tick after startup runs the pass.
- **Acceptance:** retention is documented, configurable, and works for daemons restarted more often than daily.

### T-D2: Aggregate throttled loop errors (#18) — S
- **Change:** while a step's error reports are being throttled, emit one `loop_step_error_summary` event every 10 minutes with `{step, count_since_last, last_error}`. Add it to the Grafana dashboard query that counts loop errors.
- **Tests:** a persistently failing step produces summary events with correct counts while individual events stay throttled.
- **Acceptance:** Grafana error counts match actual failure counts within one summary interval.

### T-D3: Tell users about commands sent while offline (#12) — S
- **Change:**
  - On the first poll after startup, count the updates or messages skipped (Telegram offset, Discord backlog).
  - If more than 0, reply once on that channel: "Batty restarted; ignored N message(s) sent while it was offline. Resend any commands you still need."
  - Document this in `docs/telegram.md` and the Discord docs.
- **Tests:** a fake platform with a 3-message backlog produces one notice and processes none of the 3.
- **Acceptance:** commands sent while the daemon was down are never dropped silently.

### T-D4: Telegram chunk progress per message (#25) — S
- **Change:** key chunk progress by inbox message id where available, otherwise by (chat, text hash, first-attempt timestamp). Clear the entry when a message completes, and expire entries after 10 minutes.
- **Tests:**
  - An identical message sent after a successful full send delivers all chunks.
  - A partial failure resumes at the right chunk.
- **Acceptance:** a repeated identical message never skips chunks.

---

## Stream E: lifecycle edge cases

Files: `src/team/daemon/reconcile.rs`, `src/team/daemon/deferred.rs`, `src/team/test_job.rs`, `src/team/daemon/background_tests.rs`, `src/team/daemon/agent_handle.rs`, `src/team/daemon/health/poll_shim/*`, `src/team/legacy_command_queue.rs`, `src/team/delivery/routing.rs`.

### T-E1: Cancel a pending removal when a member is re-added (#17) — S
- **Change:**
  - In `add_member`, or wherever topology reload spawns a member, if the name has a pending removal: finish the removal synchronously against the old pid only (kill and reap the old shim, keep the new state), then drop the pending entry.
  - `process_pending_removals` must match on the old shim's pid, not the member name.
- **Tests:** remove then immediately re-add `eng-1`. After the removal deadline the new shim handle, pane and worktree remain, and the old pid is reaped.
- **Acceptance:** re-adding a member never kills the new instance.

### T-E2: Wall-clock timeout for background test jobs (#19) — S
- **Change:**
  - Add `workflow_policy.verification.timeout_secs` (default 3600).
  - `poll_background_tests` and the merge-queue poll kill the job's process group when it is exceeded, and treat the result as a failed verification with the reason "timed out after Ns".
  - Show the elapsed time in `batty status`.
- **Tests:** a `sleep 30` job with `timeout_secs: 1` is killed and reported as failed, and a post-merge verification timeout triggers the revert path.
- **Acceptance:** no background verification can block the merge queue indefinitely.

### T-E3: Retire in-flight messages by id only (#21) — S
- **Change:**
  - A completion with a `message_id` removes that entry.
  - A completion without an id removes nothing and logs at debug.
  - `Ready` still clears everything. Replay sends only entries without a delivery ack.
- **Tests:** an id-less completion followed by a shim death replays every still-unacked message exactly once.
- **Acceptance:** no duplicates and no lost replays after id-less completions.

### T-E4: Bound legacy command queue retries (#22) — S
- **Change:**
  - Add `attempts` and `first_failed_at` to `QueuedCommand` (serde default).
  - Drop with a single warning after 20 attempts or 24h, and emit a `command_dropped` event.
  - Mark the `.draining` batch with a processed-index sidecar so crash recovery skips entries that were already delivered.
- **Tests:**
  - A send to a role with no channel is dropped after the limit.
  - A crash after delivering 2 of 3 entries redelivers only the 3rd.
- **Acceptance:** no command retries forever, and no duplicate after a crash.

---

## Stream F: board and init

Files: `src/task/frontmatter.rs`, `src/team/init.rs`, `src/team/templates/*`, `src/team/daemon/dispatch/mod.rs`, `src/team/task_cmd.rs`.

### T-F1: Accept upstream-tolerated frontmatter closers (#23) — S
- **Change:** accept a closing delimiter line matching `^---\s*(#.*)?$` (trailing spaces or a comment), as upstream did. Keep rejecting `----` and inline `---text`. When a file is rejected, warn once per mtime with the file path and line number.
- **Tests:**
  - Fixtures for `--- # note`, `---  ` and CRLF all parse.
  - `----` is rejected with a clear message.
  - A corpus test over the repo's own `.batty/` board fixtures parses everything that upstream parsed.
- **Acceptance:** every task file that upstream parsed still parses.

### T-F2: Preserve template comments and pins in `batty init` (#24) — M
- **Change:**
  - Replace the serde_yaml round-trip in `override_template_backend` and `set_template_name` with targeted line edits. Keep the escaping by serialising only the scalar value.
  - When changing the backend, keep `model` and `effort` pins unless they are invalid for the new backend, and print what changed.
- **Tests:**
  - Generated `team.yaml` keeps template comments.
  - A name with `:`/`#` is quoted correctly.
  - A backend switch keeps valid pins and reports dropped ones.
- **Acceptance:** `batty init --agent X --name Y` output differs from the template only in the edited lines.

### T-F3: Move a merged task to done through the legal path (#26) — S
- **Change:**
  - In the dispatch fallback `validated_move` to `done`, if the task is `in-progress` and the merge has landed, step it through `review` then `done` with attribution "daemon.merge".
  - Refuse only from states that can't legally reach `done` (`backlog`, `todo`, `blocked`), and report those to the manager as now.
- **Tests:**
  - `in-progress` with a landed merge ends in `done` with both transitions recorded.
  - `todo` is refused and reported.
- **Acceptance:** a landed merge never leaves its task in `in-progress`.

---

## Stream G: test stability and release

### T-G1: De-flake the remaining load-sensitive tests (#27) — S
- **Change:**
  - Fix `poll_shim::check_working_state_timeouts_restarts_stale_claude_management_agents` and `test_job::kill_terminates_tracked_command_process_group`. Replace their wall-clock assumptions with condition waits (`test_support::wait_until`) and injected clocks where the code compares `Instant`s.
  - Add a CI job that runs `cargo test --lib` twice with `--test-threads` set to twice the CPU count, to surface new flakes.
- **Acceptance:** 10 consecutive full-suite runs under `stress -c <ncpu>` load pass.

### T-G2: Release notes and migration guide — S
- **Depends on:** all other tasks.
- **Change:** write the "Unreleased" changelog section and `docs/migrating-to-0.12.md`. Cover:
  - every behaviour change from fork-comparison.md: config JSON, removed library modules, template defaults, git timeouts and environment variables, telemetry retention, offline chat messages, openclaw arguments, lock file semantics, validated status moves, the stricter `validate`, and deprecated config keys;
  - the upgrade order: stop old daemons before starting new ones (or rely on T-A5).
- **Acceptance:** each behaviour-change row in fork-comparison.md links to a changelog entry.

---

## Issue → task index

| Issue | Task | | Issue | Task |
|---|---|---|---|---|
| #1 | T-A1 | | #15 | T-B3 |
| #2 | T-C1 | | #16 | T-A5 |
| #3 | T-C2 | | #17 | T-E1 |
| #4 | T-C3 | | #18 | T-D2 |
| #5 | T-A2 | | #19 | T-E2 |
| #6 | T-A3 | | #20 | T-A6 |
| #7 | T-A4 | | #21 | T-E3 |
| #8 | T-A5 | | #22 | T-E4 |
| #9 | T-B1 | | #23 | T-F1 |
| #10 | T-B2 | | #24 | T-F2 |
| #11 | T-C5 | | #25 | T-D4 |
| #12 | T-D3 | | #26 | T-F3 |
| #13 | T-D1 | | #27 | T-G1 |
| #14 | T-C4 | | #28 | T-A7 |
| | | | #29 | T-A8 |
| | | | all | T-G2 |
