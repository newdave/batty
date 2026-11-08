# Batty — Agent Instructions

## What Is This Project

Batty is a hierarchical agent command system for software development. It runs a small team of agents inside tmux, routes messages between roles, tracks work on a shared Markdown board, and keeps the whole run visible in the terminal.

See `planning/architecture.md` for the system design and `planning/dev-philosophy.md` for development principles.

## Tech Stack

- **Language:** Rust
- **CLI framework:** clap
- **Terminal runtime:** tmux (pane layout, display-only surfaces when shim mode is active, session persistence)
- **Agent shim:** PTY-owning subprocess per agent, screen classification, structured socketpair protocol (`src/shim/`)
- **PTY support:** portable-pty (used by shim runtime for agent PTY management)
- **Async runtime:** tokio
- **Config format:** YAML (`.batty/team_config/team.yaml`)
- **Board format:** Markdown tasks with YAML frontmatter
- **Execution logs:** JSON lines

## Project Structure

```text
src/               # Rust source
  shim/            # Agent shim runtime (PTY, state classifier, protocol, chat)
docs/              # User and reference documentation
assets/            # Static assets (images, demos)
scripts/           # Utility scripts
planning/          # Architecture, roadmap, philosophy docs
.agents/           # Codex agent rules/skills
.claude/           # Claude agent rules/skills
.batty/
  team_config/
    team.yaml      # Team topology, routing, layout, timing
    *.md           # Role prompts used to launch architect/manager/engineer agents
    board/         # Shared task board and task files
    events.jsonl   # Team event log
```

## Development Principles

- **Compose, don't monolith.** Use existing CLI tools where possible.
- **Markdown as backend.** Keep state human-readable and git-versioned.
- **Minimal code.** Build the smallest thing that works.
- **No premature abstraction.** Prefer obvious code over clever indirection.
- **Test what matters.** Focus on tmux supervision, message routing, board state, and prompt handling.
- **Extensive unit tests.** Every module gets `#[cfg(test)]` coverage for happy paths, edge cases, and failures. Run `cargo test` before committing. If a task adds code, it adds tests.

## Test Categories

Tests are split into **unit** and **integration**:

- **Unit tests** (`cargo test`): run without tmux. Safe for CI without a tmux server.
- **Integration tests** (`cargo test --features integration`): tmux-dependent tests gated behind the `integration` Cargo feature. These require a running tmux server.
- **Note:** `tmux::tests::*` failures in nested tmux environments (e.g. inside agent sessions) are expected — not real regressions.

## Monitoring

- **Telemetry:** SQLite database at `.batty/telemetry.db` — events, agent metrics, task metrics, session summaries
- **Grafana:** `batty grafana setup` installs Grafana + SQLite plugin, provisions datasource and imports the bundled 34-panel dashboard
- **Dashboard panels:** stat counters, health gauges, hourly time series (with gap-filling for outages), pie charts, tables
- **Discord:** Three-channel bot (`#commands`, `#events`, `#agents`) with rich embeds and `$` commands
- **Telegram:** Single-channel monitoring with the same command surface

Integration tests use `#[cfg_attr(not(feature = "integration"), ignore)]`. Without the feature flag, they are automatically skipped.

## Key Dependencies

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
clap_complete = "4"
portable-pty = "0.8"
term_size = "0.3"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
toml = "0.8"
regex = "1"
anyhow = "1"
thiserror = "2"
ctrlc = "3"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
ureq = { version = "2", features = ["json"] }
```

## Building & Installing

Every code or build-input PR must bump the package version before merging or
installing a changed binary. Keep `Cargo.toml`, `Cargo.lock`, and the matching
`CHANGELOG.md` section aligned. The Version Policy check defaults to one patch
increment; use exactly one `semver:minor` or `semver:major` label for a larger
bump. Documentation-only and test-only PRs may retain the current version.
Never install changed code under an already-shipped version number.

After building, always re-sign the binary before running from `~/.cargo/bin`:

```bash
cargo build --release
cp target/release/batty ~/.cargo/bin/batty
codesign --force --sign - ~/.cargo/bin/batty
```

macOS AppleSystemPolicy (ASP) kills unsigned or stale-signed binaries. Copying over an existing binary invalidates the cached ad-hoc signature, causing ASP to SIGKILL it. `codesign --force --sign -` re-signs with a fresh ad-hoc signature.

## CLI Commands

The full command list is generated from clap: `docs/reference/cli.md` (regenerate with `./scripts/generate-docs.sh`). Do not hand-edit it.

## Development Priority: Self-Improvement and Stability

Batty is a self-improving system — agents build batty features that make agents more productive. This is the permanent priority order for all work:

1. **Stability** — green tests on main, agents don't crash, worktrees don't go stale, disk stays clean
2. **Throughput** — tasks flow from todo→done without human intervention, merges land quickly
3. **Quality** — verification loops prevent empty completions, tests cover new code, docs match shipped features
4. **Features** — Discord integration, observability, new capabilities only AFTER the above are solid

Every agent working on this project should continuously self-analyze:
- Is the test suite green? If not, fix it before writing features.
- Are agents stalling? If so, investigate and create a fix task.
- Is disk space growing? If build artifacts exceed 10GB, clean up.
- Are merges causing conflicts? If so, improve the merge pipeline.
- Are docs aligned with code? If a feature shipped without docs, write them.

**Never just report a problem — create a board task for it.** Narrating issues without creating actionable tasks is the #1 failure mode.

## Regression Delivery Gate

- For a reported bug, record the observed failure, root cause and introducing or
  contributing commit when history supports attribution. Distinguish inherited
  behavior from regressions introduced by the fork.
- Add a regression that fails before the fix and passes afterward. Test the
  public event or command path when the failure crosses modules; a helper-only
  test does not establish that the incident is fixed.
- For completion, dispatch and reconciliation changes, cover task status and
  ownership changes before work starts and while asynchronous work runs. Preserve
  unfinished branches and prove the legitimate success path still works.
- Before delivery, run focused regressions, `cargo test`, and `make lint`.
  Investigate failures; an isolated rerun alone does not prove a failure harmless.
  Run relevant tmux integration tests against a private tmux server.
- Obtain an independent review of the final diff. Resolve correctness findings
  before publishing; track deferred issues as actionable tasks.
- Keep one writer per checkout during final verification. Coordinate concurrent
  work or validate the intended commits in an isolated worktree. Source changes
  during a gate invalidate that gate for the changed code.
- Report local checks, CI, merge state, installed binary and live recovery
  separately. Do not claim runtime recovery from unit tests alone.
