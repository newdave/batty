# Fork comparison: newdave/batty vs battysh/batty

Date: 2026-10-27, second run.

Compared trees:

- **Upstream:** `battysh/batty` `main` at `5637f78` (2026-08-18).
- **Fork:** `newdave/batty` `main` at `96d2dc8`. The first comparison used fork `37cd391`; its figures appear below as "previous" where useful.

The fork is **106 commits ahead and 0 behind** upstream: 83 changes plus 23 merge commits. Since the previous comparison there are 16 new commits:
- six fix-plan streams (A–F);
- the `main()`, completion and `handle_shim_event` splits;
- the `batty status` redesign;
- accessible tmux layouts;
- test de-flaking;
- the 0.12.0 changelog and migration guide.

## Method

Same method as the first run, applied to frozen worktrees of both trees.

- **Static metrics:** a script run over `src/` in both trees. Production code is everything above the first `#[cfg(test)] mod`.
- **Default suite:** `cargo test --lib` on both trees, plus clippy.
- **Integration suite:** `cargo test --lib --features integration` on both trees. Each tree had its own private tmux server (`TMUX_TMPDIR`), so the user's tmux server was never touched.
- **Regression reviews:** three read-only reviews comparing both trees:
  - daemon and merge pipeline;
  - git, worktree, shim and tmux;
  - CLI, config and integrations.

  Each checked the status of the 29 known issues and looked for new problems, especially in code changed since `37cd391`. Every finding was checked against the code. The windows-mode pane bug (N1) was reproduced on a private tmux 3.8 server.

## Statistics

### Change volume

| Range | Commits | Files | Insertions | Deletions |
|---|---|---|---|---|
| Fork vs upstream (`5637f78..96d2dc8`) | 106 | 262 | 40,970 | 22,826 |
| Since previous comparison (`37cd391..96d2dc8`) | 16 | 125 | 18,763 | 8,304 |

Where the changes landed, by share of files changed:

| Path | Share |
|---|---|
| `src/team/` (excluding `daemon/`) | 33.5% |
| `src/team/daemon/` | 24% (of which `health/` is 5.7%) |
| `src/` top level and other dirs | 17% |
| `docs/` | 8% |
| `src/team/templates/` | 7% |
| `src/team/merge/completion/` | 5% |

### Codebase shape

| Metric | Upstream | Fork, previous | Fork, now | Now vs upstream |
|---|---:|---:|---:|---:|
| Rust files in `src/` | 198 | 224 | 248 | +50 |
| Total lines | 203,264 | 210,687 | 219,661 | +8.1% |
| Production lines | 98,899 | 101,931 | 107,027 | +8.2% |
| Test lines | 104,365 | 108,756 | 112,634 | +7.9% |
| `#[test]` functions | 3,975 | 4,080 | 4,214 | +239 |
| Feature-gated tests | 120 | 135 | 137 | +17 |
| Production functions | 3,447 | 3,555 | 3,837 | +390 |
| Functions over 100 lines | 95 | 100 | 104 | +9 |
| Functions over 200 lines | 29 | 31 | 28 | −1 |
| `TeamDaemon` fields | 67 | 27 | 27 | −60% |

The largest functions are now in the shim runtimes:

| Function | Lines |
|---|---:|
| `shim::runtime::run` | 845 |
| `run_kiro_acp` | 723 |
| `run_sdk` | 667 |
| `process_dispatch_queue` | 532 |
| `run_codex_exec_attempt` | 524 |

These used to be the largest: `main` (1,321), `handle_engineer_completion` (1,021) and `handle_shim_event` (641). They are now short dispatchers.

### Risk patterns in production code

| Pattern | Upstream | Fork, previous | Fork, now | Change |
|---|---:|---:|---:|---:|
| `Command::new("git")` | 65 | 1 | 1 | −98% |
| All `Command::new(` calls | 122 | 48 | 44 | −64% |
| Bare `ureq` calls (no timeout) | 8 | 0 | 0 | −100% |
| `Instant::now() - …` | 7 | 2 | 2 | −71% |
| `#[path = …]` | 26 | 1 | 1 | −96% |
| `allow(dead_code)` | 105 | 62 | 62 | −41% |
| `.expect(` | 98 | 56 | 56 | −43% |
| `.unwrap()` | 304 | 284 | 287 | −6% |
| Plain `fs::write` | 102 | 79 | 81 | −21% |
| `libc::kill` | 17 | 12 | 13 | −24% |
| `let _ = …` | 258 | 250 | 265 | +3% |
| `thread::sleep` | 57 | 60 | 63 | +11% |
| `unsafe {` | 36 | 37 | 40 | +11% |

The small rises in `thread::sleep`, `unsafe` and `let _` come from new process-group handling, grace-period kills, terminal-size probing (`ioctl`) and best-effort cleanup. All of these run off the daemon loop.

### Tests

| Suite | Upstream `5637f78` | Fork `96d2dc8` |
|---|---|---|
| `cargo test --lib` | 3,836 passed, **1 failed**, 119 ignored | **4,065 passed, 0 failed**, 136 ignored |
| `cargo test --lib --features integration` | 3,883 passed, **10 failed**, 63 ignored | 4,127 passed, **11 failed**, 63 ignored |
| `cargo clippy --all-targets` | 0 warnings | 0 warnings |

- **Default suite:** the upstream failure (`graceful_daemon_shutdown_times_out_before_force_kill_fallback`) is timing-dependent; the fork fixed it.
- **Integration suite:**
  - Ten failures are identical in both trees, so they are pre-existing. They are all daemon health and lifecycle tests: context exhaustion, preflight recovery, restart cwd correction, stall checkpoint and restart, the lifecycle happy path, and nudge delivery.
  - One is fork-only: `team::daemon::telemetry::tests::poll_watchers_emits_context_exhausted_event`. It fails every time (3 of 3 alone). Upstream's version of the test used the same empty role list and passed, so this is a fork regression in context-exhaustion handling.
  - CI does not run the `integration` feature, which is why none of these were caught.
- **Load stability:** under 10 CPU hogs with 20 test threads, 19 of 20 consecutive fork runs passed. The single failure, in a poll_shim stall test, could not be reproduced.

## Status of previously identified issues

| # | Issue | Status | Notes |
|---|---|---|---|
| 1 | Post-merge verify and revert read root HEAD, not the trunk ref | **Fixed** | Telemetry labels still say "main" (cosmetic) |
| 2 | `config --json` shape changed | **Fixed** | Upstream keys restored plus `schema_version`; a missing config exits non-zero (documented) |
| 3 | Library modules removed without a version bump | **Fixed** | 0.12.0, listed as breaking |
| 4 | Template auto-merge default | **Fixed** | Matches upstream; other template defaults still differ (documented) |
| 5 | Merge completion cleared the engineer's next task | **Fixed** | |
| 6 | Merge lock released before verify | **Fixed** | Lock held through verify and restore; direct and CLI merges blocked meanwhile |
| 7 | Revert hard-reset a dirty root | **Fixed** | Waits and escalates; uses `reset --keep` |
| 8 | Lock incompatible with old binaries | **Partial** | See N7 |
| 9 | Recovery fails in sub-repos with a different default branch | **Fixed** | Repeated retries can accumulate `preserved/*` branches when trunk is missing |
| 10 | Hard git timeouts | **Fixed** | Introduced N3 (lock cleanup) |
| 11 | Dead docs links | **Fixed** | `docs/reference/cli.md` is stale again (N5) |
| 12 | Offline chat commands silently dropped | **Partial** | Notice added; the count is at most 1 |
| 13 | Telemetry retention | **Fixed** | |
| 14 | openclaw argument change | **Partial** | Fallback added; see N6 and N10 |
| 15 | Git process groups and the TTY | **Fixed** | Detection is narrow (`isatty(0)`) |
| 16 | Lock errors never escalate | **Fixed** | A permanently *busy* lock still doesn't escalate (N4) |
| 17 | Re-add during pending removal | **Fixed** | Blocks the loop up to 2s while reaping |
| 18 | Throttled loop errors undercounted | **Fixed** | |
| 19 | Hung TestJob blocked the queue | **Fixed** | The timer restarts on hot reload |
| 20 | Dirty-root deferrals never escalate | **Fixed** | Deferral counters are in memory only |
| 21 | Id-less completions retired the wrong replay | **Fixed** | |
| 22 | Legacy queue retried forever | **Fixed** | A user channel that is briefly down loses messages after about 20 ticks |
| 23 | Stricter frontmatter | **Fixed (narrow)** | `--- text` and `----` closers are still rejected |
| 24 | `init` lost template comments | **Fixed** | Interactive overrides still re-serialise the YAML (bundled templates have no comments) |
| 25 | Telegram chunk keying | **Partial (mitigated)** | 10-minute TTL; inherent to the hash key |
| 26 | Fallback move to done refused | **Fixed** | Goes through review |
| 27 | Flaky tests | **Fixed** | 19/20 loaded runs; CI flake-check job added |
| 28 | Multi-repo scope fence crash | **Fixed** | |
| 29 | Deferred multi-repo merges lost on restart | **Fixed** | An erroring attempt drops the entry |

**Totals:** 24 fixed and 5 partly fixed (#8, #12, #14, #25, and #23's narrow gap). None remain unaddressed.

## New issues found in this run

Ranked by severity. All were checked in code. N1 and the integration failure were also reproduced.

| # | Severity | Issue | Evidence | Fix | Effort |
|---|---|---|---|---|---|
| N1 | **Critical** | In windows mode the daemon finds no member panes, so agents never launch. `find_pane_for_member` and `list_runtime_member_statuses` run `list-panes -t <session>` without `-s`, which lists only the current window. `auto` (the default) picks windows for teams of more than 4 agents or narrow terminals. `batty status` and the dashboard are also affected. | `team/daemon_mgmt.rs:1268`, `team/status.rs:338`; reproduced on tmux 3.8 | Add `-s` to both, plus an integration test that starts in windows mode and checks the pane map | S |
| N2 | High | The per-session `key-table` disables root-table bindings, including tmux's default mouse bindings and users' `bind -n` keys. Click-to-select, wheel scrolling, tab clicks and border drags stop working, which contradicts the on-screen hint. | `team/layout_ui.rs:266, 322`; reproduced | Drop the custom key table. Bind in root with a session guard (`if -F '#{==:#{session_name},S}' … 'send-keys …'`), or copy the root and mouse bindings into the table | M |
| N3 | Medium | After a git timeout, `remove_stale_index_lock` can delete an `index.lock` held by another git process (the user's git or IDE, or another daemon path). It also runs for subcommands that never take the index lock. | `src/git.rs:446, 696-730` | Clean up only after index-writing subcommands; recheck that the lock is unchanged; otherwise only report it | S |
| N4 | Medium | A permanently busy merge lock stalls the queue with no escalation. Examples: a reused PID in a stale marker, a hung `batty merge`, an old binary. Upstream timed out after 60s and retried. | `daemon/merge_queue.rs:413-417, 892-897`; `merge/lock.rs:124` | Track how long the lock has been busy; escalate after N minutes, naming the marker's PID | S |
| N5 | Medium | `docs/reference/cli.md` was not regenerated for `batty attach [MEMBER]`, so the CI "generated docs are committed" check will fail. | `docs/reference/cli.md:182` vs `src/cli.rs:68-72` | Run `scripts/generate-docs.sh` and commit | XS |
| N6 | Medium | `batty start` now refuses an unknown `channel:` or an empty Telegram target, because `validate()` hard-fails. The changelog lists this under "Added" and the migration guide doesn't mention it. | `team/config/mod.rs:457`, `daemon_mgmt.rs:941` | Add a Breaking entry and a guide note, or make it a warning for `start` | S |
| N7 | Medium | Mixed versions: an upstream holder keeps an empty `merge.lock` through minutes of synchronous verify. The fork treats an empty file as free after 5s, so both can merge. | `merge/lock.rs:35-39, 262-275` | Use a much longer grace period for empty files (about 1h), or document that mixed versions are unsupported | S |
| N8 | Medium | The verify worktree (`.batty/verify-worktree`) gets a private cargo home and target dir (cold build, offline failure), and nothing cleans it up. Setup errors and the TestJob timeout count as a failed verify, which reverts a good merge. This only applies when the root isn't on trunk. | `daemon/merge_queue.rs:2265-2322, 1035-1039`; `team/task_loop.rs:192-216` | Use the shared cargo home and target dir for the verify dir; treat setup errors as retry-then-escalate; clean up in disk hygiene | S–M |
| N9 | Medium | Fork-only integration test failure: `poll_watchers_emits_context_exhausted_event`. | `daemon/telemetry.rs:1889` | Bisect the context-exhaustion path (likely the stall/retry or shim-event refactors) and fix | S |
| N10 | Low-med | `channels.rs` runs `<provider> --version` with no timeout inside `validate()`, on every start and preflight. | `team/config/channels.rs:170` | Bound it with a timeout, or probe only in `batty validate` | S |
| N11 | Low-med | Members added at runtime ignore the layout mode. They split into another member's window and get no window options or jump key. | `daemon/reconcile.rs:570-600` | Use `new_window_pane` in windows mode and re-apply the UI | S–M |
| N12 | Low | The `auto` size check reads `stty size` from stdin, so a non-TTY start assumes 220×50. | `team/layout_ui.rs:72-81` | Probe `/dev/tty` | S |
| N13 | Low | `status --wide` is still width-fitted when piped, so cells can be truncated. Upstream printed full rows. | `team/status_render.rs` (`fit_widths`) | Skip fitting for `--wide` when stdout isn't a TTY | S |
| N14 | Low | `status --watch` with no config prints "status unavailable" forever instead of exiting. | `team/session.rs` watch loop | Exit on the first error when not on a TTY | XS |
| N15 | Low | The openclaw fallback triggers only on commander-style error text. Other `--to`-only providers fail permanently, and the fallback flag never re-probes. | `team/comms.rs:287-292` | Broaden detection ("unknown", "unrecognized" plus the flag) | S |
| N16 | Low | The process-wide `TestJob` default timeout is global state, so parallel tests with different configs race on it. After a daemon crash mid-verify, `batty merge` is blocked with no override. | `daemon.rs:489`, `merge_queue.rs:1670` | Pass the timeout explicitly; add `batty merge --force` or show stale in-flight state in `batty status` | S |
| N17 | Low | Docs drift: the changelog and guide say the offline notice reports "how many" messages (it's at most 1), and that `init --agent` keeps comments (interactive overrides drop them). `replenishment_threshold` is missing its `automation.` prefix. `verification.timeout_secs` and `layout.keys` are missing from config-reference. | `CHANGELOG.md`, `docs/migrating-to-0.12.md`, `docs/config-reference.md` | Reword and add the missing entries | XS |

Smaller follow-ups noted by reviewers:
- A deferred merge is pushed to the front of the queue and holds up unrelated tasks behind it.
- A failed `attempt_direct_merge` drops the deferred entry.
- Archiving with a missing trunk can add another `preserved/*` branch on every retry.

## Assessment

- **Stability and correctness:** much better than upstream. All daemon-loop blocking, data-loss and fail-open issues from the first review are fixed. The default suite is fully green, where upstream has a failure.
- **New features:** the tmux layout work introduced the only critical issue (N1). Combined with N2, the default `auto` layout is not ready: on a team of more than 4 agents it starts no agents, and mouse use breaks. Until N1 and N2 are fixed, `layout.mode: panes` is a safe workaround, and should arguably be the default.
- **Compatibility:** documented and versioned (0.12.0). The remaining gaps are N6 (`start` refuses bad channel config) and N7 (mixed-version locking).
- **Test coverage gap:** CI never runs the `integration` feature. That let N1-style tmux bugs, the fork-only integration failure, and 10 pre-existing upstream failures go unnoticed.

## Recommended next steps

1. **Hotfix:** fix N1 (add `-s` and an integration test) and N5 (regenerate the CLI docs). Until N2 is fixed, default `layout.mode` to `panes` or keep the root key table.
2. **tmux UX:** fix N2 (bindings in the root table with session guards), N11 (runtime adds) and N12 (probe `/dev/tty`).
3. **Merge pipeline:** fix N4 (busy-lock escalation), N7 (empty-file grace period), N8 (verify worktree cargo env, setup-error handling and cleanup) and N16.
4. **Git safety:** fix N3 (index-lock cleanup).
5. **Compatibility and docs:** fix N6, N10, N15 and N17.
6. **Tests and CI:**
   - fix N9 and the 10 pre-existing integration failures;
   - add a CI job that runs `--features integration` against a private tmux server;
   - add a windows-mode startup scenario test.
7. **Structural follow-ups:**
   - a shared `StructuredRuntime` for the shim runtimes (the largest functions are now there);
   - split `automation.rs` and `status.rs`;
   - rename the three `verification` modules;
   - remove the remaining item-level `allow(dead_code)`.
