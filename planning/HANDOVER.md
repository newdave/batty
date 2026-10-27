# Handover: batty fork work status

Last updated: 2026-10-27. Repository: `~/Projects/batty`. Fork remote: `origin` = `newdave/batty`. Upstream: `upstream` = `battysh/batty` (`main` at `5637f78`, unchanged since 2026-08-18).

## Ground rules from the user

- Work **only on the batty fork**. Never read or change projects batty supervises (e.g. `~/Projects/terrain-link`). Never touch the user's default tmux server; tests use a private server via `crate::tmux::tmux_command()` (`TMUX_TMPDIR` is pinned under `cfg(test)`).
- Don't install to `~/.cargo/bin` unless asked; that hot-reloads running batty daemons.
- **Commits:** small, one concern each. Plain-sentence subject, a body explaining what broke and what changed, no Conventional-Commit prefixes, no AI attribution.
- **Staging:** stage explicit paths (no `git add -A`).
- **Git hook** (`~/.claude/hooks/block-dangerous-git.sh`):
  - Git commands may not contain shell variables, heredocs, `$`, or backticks in `-m` messages. Write messages to a file and use `git commit -F <file>`.
  - `git branch -D` is blocked; `-d` works.
- **Settled decisions:**
  - `auto_merge.enabled` matches upstream: templates set no `auto_merge` block, and the interactive init prompt defaults to "no".
  - The project-rename feature was dropped.

## State of `main`

- **Pushed:** `origin/main` includes everything below (see `git log origin/main`).
- **Round 2 commits** (all pushed with this handover; full suite green at each):
  - `222b224` Round 2 fix plan (`planning/fork-fix-plan-2.md`)
  - `180f55a` R3: index.lock cleanup only when clearly ours; reuse preserved/* archive
  - `d0a21c9` R4: start warns on channel config; status `--wide`/`--watch` when piped; openclaw fallback; real offline-backlog counts; docs
  - `18f6579` R2: busy-lock escalation, 1h empty-lock grace, verify worktree hardening, `batty merge --force-clear-stale`, queue/backoff residuals
  - `96707cd` R1: F-keys in the root table acting only in batty sessions (mouse and user bindings keep working); defaults back to `layout.mode: auto` and `layout.keys: true`; blank tab bar fixed; `panes` without zones fixed; runtime adds follow the layout (N11); terminal size from `/dev/tty` (N12). Verified with a real attached client on a private tmux server; the default server was untouched. Full suite: 4,108 passed.
- **Still running when this was written: stream R5** (integration tests and CI), in worktree `.claude/worktrees/agent-a1e77df76af67d1b9` (branch `worktree-agent-a1e77df76af67d1b9`, based on `222b224`). Scope, per `planning/fork-fix-plan-2.md`:
  - R5.1: fork-only failure `team::daemon::telemetry::tests::poll_watchers_emits_context_exhausted_event`.
  - R5.2: the 10 integration failures shared with upstream.
  - R5.3: a CI job for `--features integration` with a job-local `TMUX_TMPDIR`.
  - R5.4: a windows-mode daemon startup scenario test.

  If the agent's result is lost, export its uncommitted diff with `git -C <worktree> add -A && git -C <worktree> diff --cached -M > r5.patch`, then `git apply -3 r5.patch` on `main`. Run the integration suite only against a private tmux server: `env -u TMUX TMUX_TMPDIR=/tmp/<dir> cargo test --lib --features integration`.

## What remains (in order)

1. **Merge R5** when it finishes. Run the full suite plus the integration suite on a private tmux server, then commit.
2. **CHANGELOG:** add the round-2 lines below to the `## 0.12.0 — Unreleased` section of `CHANGELOG.md`. Also correct the layout bullets there, which still describe the hotfix defaults (`panes`, keys off): R1 restored `auto` and keys on.
3. **De-flake** `git::tests::timeout_keeps_index_lock_written_by_a_concurrent_process` (R3). It failed once at load average ~120 and passes 5/5 alone. Also watch `merge_queue::post_merge_verification_runs_off_the_poll_loop`, `health::checks::reconcile_preserves_dirty_work…`, and `worktree::tests` `reset_worktree_to_base_archives…` (5s git timeout under load).
4. **Push** `main` to `origin`.
5. **Clean up test leftovers** under `/tmp/bt-*` (about 266 private tmux test dirs; their owning processes are dead). They belong to batty test runs, not the user. For each: `env -u TMUX TMUX_TMPDIR=<dir> command tmux kill-server` then remove the dir. Never run a bare `tmux kill-server`.
6. **Third comparison pass** (optional, as requested earlier): same method as `planning/fork-comparison.md`. Metrics script at `/tmp/claude-scripts/metrics.py` (recreate if `/tmp` is cleared). Frozen worktrees go under `/private/tmp/batty-upstream` and `/private/tmp/batty-fork-snapshot`; don't use `/private/tmp/batty-fork`, which is an unrelated pre-existing clone.
7. **Open items not in round 2:**
   - a shared `StructuredRuntime` for the shim runtimes (the largest functions are now `shim::runtime::run` at 845 lines, `run_kiro_acp` at 723 and `run_sdk` at 667);
   - split `automation.rs` and `status.rs`;
   - rename the three `verification` modules;
   - remove the ~62 item-level `allow(dead_code)`;
   - the remaining partial issues in `fork-comparison.md` (#8 residual, #12 Discord window, #23 narrow closers, #25 hash keying).

## Round-2 changelog lines (not yet in CHANGELOG.md)

R3:
- Fixed: after a git timeout, `index.lock` is removed only when clearly batty's own: a timed-out index-writing command in its own, now-gone process group, lock modified during the run and unchanged on recheck. Otherwise a warning is logged. Timed-out fetch/push never touch it.
- Fixed: recovery with a missing trunk reuses an identical `preserved/*` branch instead of creating one on every retry.

R4:
- Changed: `batty start` warns about an unknown channel or empty Telegram target instead of refusing; `batty validate` still reports them as errors.
- Changed: the channel provider `--version` probe runs only in `batty validate`, with a 5s timeout, once per process.
- Fixed: `status --wide` prints full rows when piped; `status --watch` exits non-zero on error when piped.
- Fixed: the openclaw `--to` fallback recognises more error wordings and retries the modern form daily.
- Fixed: the offline-messages notice reports the real count (Telegram pending updates; Discord newest 100), capped at 100+.

R2:
- Added: a merge lock that stays busy escalates after `auto_merge.lock_busy_escalate_after_secs` (default 600), naming the holder PID, then hourly.
- Changed: an empty or unreadable `merge.lock` (older binaries) is treated as held for up to 1h, not 5s. Don't run old and new binaries' merges in the same project at once.
- Fixed: post-merge verification in the verify worktree uses the shared cargo home and `shared-target/verify`; setup failures are retried and escalated, never reverted; setup time no longer counts against the test timeout.
- Fixed: `.batty/verify-worktree` is removed during disk hygiene and on `batty stop`.
- Added: `batty merge --force-clear-stale` clears a merge left in verification with no daemon running; `batty status` flags it.
- Fixed: a merge request in retry backoff no longer blocks other requests; failed multi-repo direct merges are kept and retried with backoff (#29); merge telemetry and logs name the configured trunk.

R1:
- Changed: batty's F-keys are bound in tmux's root table and act only in batty sessions. Mouse clicks, wheel scrolling, tab clicks and your own `bind -n` keys keep working, several teams can share a tmux server, and the keys are removed when the last batty session stops. The keys need tmux 3.2.
- Changed: `layout.mode` defaults to `auto` again and `layout.keys` to `true`.
- Fixed: the window tab bar was blank in batty sessions, so tabs could not be clicked.
- Fixed: `layout: {mode: panes}` without `zones` started no member panes.
- Fixed: members added while the team runs follow the layout mode and get jump keys.
- Fixed: `auto` reads the terminal size from `/dev/tty`.

R5: pending. Take its lines from its report.

## Key documents

- [fork-comparison.md](fork-comparison.md): second comparison with upstream (metrics, tests, the status of issues #1–#29, new findings N1–N17, hotfix status).
- [fork-fix-plan.md](fork-fix-plan.md): round 1 plan (complete).
- [fork-fix-plan-2.md](fork-fix-plan-2.md): round 2 plan. R1–R4 are done and committed; R5 is in progress.
- `CHANGELOG.md` "0.12.0 — Unreleased" and `docs/migrating-to-0.12.md`: user-facing release notes.
- `docs/tmux-layouts.md`: layout modes and keys.
