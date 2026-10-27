# Fork fix plan, round 2

Date: 2026-10-27. Base: `5860a3e`.

This round fixes the open findings from the second comparison run ([fork-comparison.md](fork-comparison.md#new-issues-found-in-this-run)): N2–N4 and N6–N17, plus the residual gaps in #9, #12 and #29. N1 and N5 were fixed in the hotfix.

Rules are the same as round 1:
- tests that fail before the fix and pass after;
- clippy, the all-features build and the full suite stay green;
- agents report proposed changelog lines instead of editing `CHANGELOG.md`.

Streams own disjoint files and run in parallel.

## Stream R1: tmux layouts (N2, N11, N12)

Files: `src/team/layout.rs`, `src/team/layout_ui.rs`, `src/tmux.rs`, `src/team/daemon/reconcile.rs` (member add path only), `src/team/config/types.rs` (`LayoutConfig` only), `docs/tmux-layouts.md`, the layout sections of `docs/config-reference.md`, README and getting-started.

- **R1.1 (N2): keep tmux's own bindings working.**
  - Drop the per-session `key-table`.
  - Install batty keys in the root table, guarded by session name. Example: `bind -n F2 if -F '#{==:#{session_name},<S>}' '<action>' 'send-keys F2'`. With several batty sessions, use one binding per key that dispatches on a session option (e.g. `@batty_key_F2`), so sessions don't overwrite each other.
  - Remove the bindings on `batty stop` only when no other batty session uses them.
  - Never touch `prefix ?` or any key batty doesn't own.
  - **Acceptance**, checked on a private tmux server with a real attached client:
    - mouse click-to-select, wheel scroll and tab clicks work;
    - the user's own `bind -n` keys work in batty and non-batty sessions;
    - F-keys work only in batty sessions and pass through elsewhere;
    - two batty sessions don't clobber each other.
- **R1.2: restore the intended defaults once R1.1 passes.** That means `layout.mode: auto` and `layout.keys: true`. Update the docs and the caveats added in `dee89ae`.
- **R1.3 (N11): members added at runtime follow the layout mode.** Windows mode creates a new window with the window options. Focus mode re-applies its layout. The jump-key mapping and help are refreshed.
- **R1.4 (N12): read the terminal size from the controlling terminal.** Use `/dev/tty`, falling back to stdin, then 220×50, so a non-TTY start doesn't assume a huge terminal.

## Stream R2: merge pipeline (N4, N7, N8, N16, #29 residuals)

Files: `src/team/daemon/merge_queue.rs`, `src/team/merge/lock.rs`, `src/team/merge/completion/**`, `src/team/test_job.rs`, `src/team/daemon/background_tests.rs`, the cargo-env helpers in `src/team/task_loop.rs`, `src/team/daemon/health/disk_hygiene.rs`, `src/commands/*` (`batty merge` flags only).

- **R2.1 (N4):** track how long the merge lock has been busy. After 10 minutes (configurable), escalate once with the holder PID from the marker, if any, and then every hour. This applies to both the queue and in-flight revert paths.
- **R2.2 (N7):** treat an empty `merge.lock` held by another process as held for up to 1h (`UNOWNED_LOCK_STALE_AFTER`), not 5s. Keep the PID marker semantics. Document that mixed old/new binaries should not merge at the same time.
- **R2.3 (N8): the verify worktree.**
  - Use the shared cargo home and a shared target subdirectory (`shared-target/verify`).
  - Treat setup errors (`worktree add`/`checkout` failure, `index.lock`) as retryable: retry with backoff, then escalate, and never revert.
  - The TestJob timeout must not include setup time.
  - Remove or prune `.batty/verify-worktree` in disk hygiene and on stop.
- **R2.4 (N16):**
  - Pass the TestJob timeout explicitly at every spawn site and remove the process-wide default.
  - Add `batty merge --force-clear-stale` (or similar). It clears a persisted in-flight merge whose daemon is gone, after confirming no daemon is running, and prints what it cleared.
  - Show a stale in-flight merge in `batty status`.
- **R2.5:** small residuals:
  - a deferred request no longer blocks unrelated requests behind it (re-queue it behind them, or skip to the next eligible);
  - a failed `attempt_direct_merge` keeps the entry with backoff instead of dropping it (#29);
  - telemetry and log labels say the configured trunk, not "main".

## Stream R3: git safety (N3, #9 residual)

Files: `src/git.rs`, `src/worktree/ops.rs`, `src/worktree.rs`.

- **R3.1 (N3): only clean up `index.lock` when it is clearly ours.**
  - Only after a timed-out subcommand that writes the index (`add`, `commit`, `reset`, `checkout`, `merge`, `rebase`, `stash`, `cherry-pick`, `revert`, `apply`).
  - Only when its process group was ours and is gone, the lock's mtime falls within the command's run, and a short recheck shows the lock unchanged (size and mtime).
  - Otherwise warn and leave it.
  - Tests: a lock created by a concurrent process during the run is kept; a lock left by our killed `add` is removed; a timed-out `fetch` never touches the lock.
- **R3.2 (#9 residual):** when trunk is missing, don't create a new `preserved/*` branch on every recovery retry if an identical one already points at the same commit. Reuse it.

## Stream R4: compatibility, CLI and docs (N6, N10, N13, N14, N15, N17, #12)

Files: `src/team/config/channels.rs`, `src/team/config/mod.rs` (validate paths), `src/team/daemon_mgmt.rs` (start-time validation call only), `src/team/comms.rs`, `src/team/status_render.rs`, `src/team/session.rs` (status watch loop), `src/team/telegram.rs` and `src/team/discord.rs` (first-poll backlog count only), `docs/migrating-to-0.12.md`, `docs/config-reference.md` (non-layout sections), `docs/telegram.md`, `docs/discord.md`.

- **R4.1 (N6):** `batty start` warns about an unknown channel or empty Telegram target rather than refusing. `batty validate` still reports these as errors. The start path never runs the provider `--version` probe.
- **R4.2 (N10):** run `<provider> --version` with a 5s timeout, once per process (cached). Only `batty validate` runs it.
- **R4.3 (N13):** `status --wide` prints full-width rows (no fitting or truncation) when stdout is not a TTY.
- **R4.4 (N14):** `status --watch` exits non-zero on the first error when stdout is not a TTY. On a TTY it keeps retrying but shows the error.
- **R4.5 (N15):** broaden openclaw fallback detection to "unknown", "unrecognized", "unexpected" or "invalid" plus `--channel`/`--target`. Re-probe the modern form once per day.
- **R4.6 (#12):** count the real offline backlog on the first poll.
  - Telegram: fetch with `offset=0` and a limit, count the updates, then acknowledge past the last one.
  - Discord: fetch up to 100 recent messages after the last-seen id and count them.
  - The notice reports the real number, capped at "100+".
- **R4.7 (N17): docs accuracy.** The offline-notice wording should match R4.6. Note that `init --agent` comment preservation applies to `--agent` only. Use `automation.replenishment_threshold`. Add `workflow_policy.verification.timeout_secs` and `telemetry.retention_days` to `config-reference.md`. List the `start` behaviour from R4.1.

## Stream R5: integration tests and CI (N9, pre-existing integration failures)

Files: tests under `src/team/daemon/health/**`, `src/team/daemon/telemetry.rs` (tests), `src/team/daemon/tests.rs`, `src/team/test_support.rs`, `.github/workflows/ci.yml`. Minimal production fixes in health code if a test exposes a real bug, with the reason given.

- **R5.1 (N9):** find why `poll_watchers_emits_context_exhausted_event` fails on the fork (it passes upstream with the same fixture) and fix the root cause, production or test.
- **R5.2:** fix the 10 integration failures shared with upstream:
  - context exhaustion relaunch;
  - preflight missing-session recovery;
  - restart cwd correction (×2);
  - pane-dead respawn;
  - stall checkpoint (×3);
  - lifecycle happy path;
  - nudge live delivery.

  For each, say whether it was a test-fixture problem (e.g. "missing role definition for member") or a real bug.
- **R5.3:** add a CI job that runs `cargo test --lib --features integration` with tmux installed and `TMUX_TMPDIR` pointing at a job-local directory.
- **R5.4:** add a windows-mode startup scenario that builds the layout and runs the daemon's pane-map rebuild and launcher pane lookup, so N1-class bugs fail in CI.

## Order

R1–R5 run in parallel. They are merged in the order R3, R4, R2, R1, R5, with the full suite after each. R5 runs the integration suite again after every other stream has landed. A `CHANGELOG.md` update and a short third comparison pass follow.
