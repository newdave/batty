# Task Breakdown: TypeSafe AI Jev for supervisory decisions

**Source PRP**: [docs/prps/typesafe-jev-decisions.md](../prps/typesafe-jev-decisions.md)
**Brainstorm**: [docs/brainstorming/2026-11-07-typesafe-jev-decisions.md](../brainstorming/2026-11-07-typesafe-jev-decisions.md)
**Template**: [docs/templates/technical-task-template.md](../templates/technical-task-template.md)

## Feature Overview

Batty's supervisory yes/no and choose-one decisions run on regexes, keywords and commit counts today. This feature routes five of them through TypeSafe AI's Jev decision model (`typesafe-ai/jev-1.14.0`, `POST /v1/decisions` on the Vercel AI Gateway):

- completion checks (veto only);
- a review rubric (advisory);
- manager and architect inbox triage;
- manager and architect PTY screen state;
- stall reasons when the heuristic finds no signal.

Everything sits under `workflow_policy.decisions` and is off by default (`provider: heuristic`). Every path falls back to today's heuristic in the same poll. No network call runs on the poll thread, and no secret is sent or logged.

The 12 tasks below map one-to-one onto the PRP's "List of tasks, in order". None is split. S is under about 200 changed lines, M is about 200 to 600.

## Shared Rules (apply to every task)

- **Stack**: Rust, no new dependencies. `regex`, `sha2`, `ureq`, `serde_json` and `rusqlite` already exist.
- **Tests**: every change gets `#[cfg(test)]` coverage for happy path, edge cases and failures. Unit tests run without tmux. Tmux-dependent tests use `#[cfg_attr(not(feature = "integration"), ignore)]`. No `allow(dead_code)`.
- **Poll thread never blocks**: every Gateway call is a `TestJob`. `orchestrator_only_reason`, `supervisory_progress_signal`, `on_state_changed` and `finish_engineer_completion` only read cached verdicts from `daemon.decisions`.
- **Redact first**: call `decide::redact::redact` before hashing, sending, logging or storing in a `TeamEvent`. Never log raw pane content, message bodies, diffs or the API key. Never put the key in a URL. Redact ureq `Transport` errors.
- **Fallback is today**: a missing key, budget cap, timeout, HTTP error, invalid answer, refusal or confidence below `min_confidence` gives the heuristic outcome plus one `decision_fallback` event.
- **Builder default**: `TestDaemonBuilder` sets `decisions.provider = Heuristic` (from JEV-01 on). New tests opt in with `.workflow_policy(..)` and a fake endpoint.
- **Validation commands** (every task; swap the filter for the one listed per task):

```bash
export CARGO_TARGET_DIR=target
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --lib <filter>
cargo test --lib            # full lib suite before merge
```

- **Integration (tmux-gated) tests**: run with `env -u TMUX TMUX_TMPDIR=$(mktemp -d) cargo test --lib --features integration <filter>`.
- **Definition of Done**: all validation commands green; with `provider: heuristic` behaviour is unchanged and no HTTP call is made; every Jev outcome emits `decision_made` or `decision_fallback`; a planted secret never appears in `events.jsonl`, the orchestrator log, the shim event log or tracing output.

---

## JEV-01: Add DecisionsConfig to WorkflowPolicy

**Priority**: Critical | **Size**: S | **Depends on**: none | **Parallel with**: none (start of chain)

**Purpose**: As an operator, I need one opt-in config block with safe defaults and a per-path enable list.

**Files**
```
├── src/team/config/types.rs - DecisionProvider { Heuristic (default), Jev }, DecisionPath { Completion, Rubric,
│     Triage, ScreenState, Stall }, DecisionTimeouts, DecisionsConfig (mirror BenchRecoveryConfig 492-550);
│     #[serde(default)] pub decisions on WorkflowPolicy (255) and its Default (445-446)
├── src/team/config/tests_advanced.rs - defaults + partial override (mirror 534-568)
└── src/team/test_support.rs - TestDaemonBuilder sets decisions.provider = Heuristic explicitly
```

**Acceptance Criteria**
```gherkin
Scenario: Defaults
  Given a team.yaml with no decisions key
  When the config is parsed
  Then workflow_policy.decisions.provider == DecisionProvider::Heuristic
  And model == "typesafe-ai/jev-1.14.0", endpoint == "https://ai-gateway.vercel.sh/v1/decisions"
  And api_key_env == "AI_GATEWAY_API_KEY", daily_spend_cap_usd == 5.0, min_confidence == 0.6
  And zero_data_retention == true
  And timeouts_ms is screen_state 2000, triage 2000, stall 2000, completion 10000, rubric 10000
  And paths contains all five DecisionPath values

Scenario: Partial override
  Given decisions: { provider: jev, paths: [triage] }
  Then provider == Jev, paths == [Triage], and every other field keeps its default

Scenario: Builder default
  When TestDaemonBuilder::new(..).build() runs without .workflow_policy
  Then the daemon policy has decisions.provider == Heuristic
```

**Tests (unit)**: the three scenarios.

**Validation filter**: `cargo test --lib config`

**Risks**: The API key is read only from the env var named in `api_key_env`, never from `team.yaml`. Without the builder default, later tasks could change existing tests.

---

## JEV-02: Create the secret redaction module

**Priority**: Critical | **Size**: M | **Depends on**: JEV-01 | **Parallel with**: none

**Purpose**: As the daemon, I need every secret replaced before any content leaves the process or reaches a log.

**Files**
```
├── src/team/decide/mod.rs (CREATE) - module root (types land in JEV-03..05)
├── src/team/decide/redact.rs (CREATE) - OnceLock<Vec<(kind, Regex)>>; redact(text) -> String
└── src/team/mod.rs - pub mod decide;
```

**Patterns**: `http::redact_secret` (literal redaction precedent).

**Acceptance Criteria**
```gherkin
Scenario: Each pattern is replaced
  Given text containing an sk-/sk-ant- key, a gh[pousr]_ or github_pat_ token, an xox[abprs]- token,
    AKIA[0-9A-Z]{16} or aws_secret_access_key=..., an AIza key, a JWT, a PEM PRIVATE KEY block,
    scheme://user:pass@host, a Bearer token, an env line like "export API_TOKEN=abc", a >=40-char base64 run
  When redact(text) runs
  Then each match becomes "[REDACTED:<kind>]" and surrounding text is unchanged

Scenario: False positives
  Given ordinary Rust code and a bare 40-hex git SHA
  Then redact returns the text unchanged (hex runs need >= 48 chars)

Scenario: Idempotence
  Then redact(redact(text)) == redact(text)
```

**Tests (unit)**: one per pattern, false-positive cases, idempotence.

**Validation filter**: `cargo test --lib decide::redact`

**Risks**: Over-broad patterns hide useful content from Jev. Under-broad ones leak secrets. Keep the regex set compiled once.

---

## JEV-03: Create the Gateway Decisions client

**Priority**: Critical | **Size**: M | **Depends on**: JEV-02 | **Parallel with**: none

**Files**
```
├── src/team/decide/client.rs (CREATE) - build_request, parse_response, parse_error (pure); send(endpoint,
│     key, body, timeout) via crate::team::http::agent().post(..).timeout(..)
└── src/team/decide/mod.rs - Question, Answer (serde tag "type", Refusal, #[serde(other)] Unknown),
      DecisionResponse { answers, cost_usd, input_tokens }, Fallback enum
```

**Patterns**: telegram.rs:180-222 (request, Status vs Transport redaction); `parse_send_message_response` test (telegram.rs:1052).

**Acceptance Criteria**
```gherkin
Scenario: Request shape
  When build_request("typesafe-ai/jev-1.14.0", input, [predicate, choice, score], true) runs
  Then the JSON has model, input, questions with "type" predicate|choice|score,
    and providerOptions.gateway.zeroDataRetention == true

Scenario: Response parsing
  Given a body with provider_metadata.gateway.cost "0.000011844"
  Then parse_response gives cost_usd == 0.000011844 parsed from the string
  And a {"type":"refusal"} answer parses as Answer::Refusal, an unknown type as Answer::Unknown
  And invalid JSON gives Fallback::InvalidAnswer

Scenario: Errors
  Given status 401, 402, 403, 429 or 5xx
  Then send returns Fallback::Http(code) with no retry
  And a request exceeding the timeout returns Fallback::Timeout
```

**Tests (unit)**: request shape per question type; parse success, refusal, unknown, invalid JSON; error mapping; a `std::net::TcpListener` thread server for one round trip and one timeout case.

**Validation filter**: `cargo test --lib decide::client`

**Risks**: `Request::timeout` is an overall deadline in ureq 2. Never use `/v1/chat/completions` or `/v1/evaluate`. Never log the error body, the URL or the key. Cap questions at 16 per request.

---

## JEV-04: Add the daily budget and the change gate

**Priority**: Critical | **Size**: S | **Depends on**: JEV-03 | **Parallel with**: none

**Files**
```
├── src/team/decide/budget.rs (CREATE) - add_spend(conn, date, usd), spend_on(conn, date), over_cap
├── src/team/telemetry_db.rs init_schema (254-357) - decision_spend(date TEXT PRIMARY KEY,
│     usd REAL NOT NULL DEFAULT 0, calls INTEGER NOT NULL DEFAULT 0); upsert ON CONFLICT(date)
└── src/team/decide/gate.rs (CREATE) - ChangeGate keyed by (DecisionPath, key) storing (hash, answer);
      hash = sha2(redacted input + question set id); triage LRU capped at 256
```

**Patterns**: upsert at telemetry_db.rs:536, 620; `record_review_queue_snapshot` (1281-1325); `open_in_memory` (248).

**Acceptance Criteria**
```gherkin
Scenario: Spend accumulates per UTC day
  Given open_in_memory
  When add_spend(conn, "2026-11-07", 0.01) runs twice
  Then spend_on("2026-11-07") == 0.02 with calls == 2, and spend_on("2026-11-08") == 0.0

Scenario: Gate hit
  Given ChangeGate holds a hash for (Triage, "msg-1")
  When the same redacted input and question set are checked
  Then unchanged() returns true; a changed input returns false

Scenario: LRU bound
  When 257 triage entries are inserted
  Then the cache holds 256 and the oldest is evicted
```

**Tests (unit)**: the three scenarios.

**Validation filter**: `cargo test --lib decide::budget` and `cargo test --lib decide::gate`

**Risks**: Caches must stay bounded: one entry per (path, member) plus the 256-entry triage LRU. Hash only redacted input.

---

## JEV-05: Create the per-path question builders, mappers and criteria parser

**Priority**: Critical | **Size**: M | **Depends on**: JEV-04 | **Parallel with**: none

**Files**
```
├── src/team/decide/paths.rs (CREATE) - screen/triage/completion/rubric/stall questions + mappers;
│     apply_confidence(answer, min); input cap 96K chars (truncate head, keep tail for screens)
└── src/team/decide/criteria.rs (CREATE) - parse_acceptance_criteria(task_body) -> Vec<String>
```

**Acceptance Criteria**
```gherkin
Scenario: Screen mapping
  Given a choice answer "context_exhausted" with confidence 0.8
  Then the mapper returns ScreenOutcome::ContextExhausted (idle|working|context_exhausted|unknown 1:1)

Scenario: Triage, completion, rubric, stall
  Then triage maps to TriageOutcome::{Actionable, StatusReply, Noise, Escalation}
  And completion predicates done/narration_only map to CompletionOutcome
  And rubric scores use levels ["not met","partly met","met"] plus predicate criteria_met
  And stall maps predicate stuck + choice waiting_on_input|blocked_on_dependency|looping|idle_without_work|other

Scenario: Rejections
  Given a Refusal, an Unknown answer, or confidence 0.5 with min_confidence 0.6
  Then the mapper returns Fallback::Refusal, Fallback::InvalidAnswer, or Fallback::LowConfidence

Scenario: Criteria parser
  Given a task body with 12 bullets under "## Acceptance Criteria" (spec_gen.rs:124, 241)
  Then 10 criteria are returned; a body without the section returns an empty Vec
```

**Tests (unit)**: every mapper including refusal, unknown and low confidence; parser with and without the section; the 96K cap.

**Validation filter**: `cargo test --lib decide::paths` and `cargo test --lib decide::criteria`

**Risks**: 32K context: send a diff summary (stat, file list, first hunks), never the full diff. Screens send the last 100 lines.

---

## JEV-06: Build the daemon job plumbing and decision events

**Priority**: Critical | **Size**: M | **Depends on**: JEV-05 | **Parallel with**: none (last foundation)

**Files**
```
├── src/team/daemon/decisions.rs (CREATE) - DecisionsState; decisions_enabled, start_decision, poll_decisions
├── src/team/daemon.rs - decisions field (294-341), constructor (560), mod decisions;
├── src/team/daemon/poll.rs - run_recoverable_step("poll_decisions", ..) before check_working_state_timeouts (276)
└── src/team/events.rs - decision_made, decision_fallback, decision_budget_exceeded, completion_vetoed,
      review_rubric (..Self::base pattern, 148) + serialization tests (1970)
```

**Patterns**: `TestJob::spawn_with_timeout` / `try_take` (test_job.rs:94-245, pgid ignored); `BackgroundTests` (background_tests.rs:19-73).

**Acceptance Criteria**
```gherkin
Scenario: Disabled
  Given provider: heuristic
  When start_decision(Triage, ..) runs
  Then no job is spawned and no HTTP call is made

Scenario: No key
  Given provider: jev and AI_GATEWAY_API_KEY unset
  When several polls run
  Then exactly one decision_fallback with reason "no_key" is emitted, then the heuristic stays silent

Scenario: Budget
  Given decision_spend for today >= daily_spend_cap_usd
  Then no job is spawned, decision_fallback reason "budget" is emitted,
    and decision_budget_exceeded is emitted once per UTC day

Scenario: Result applied on a later tick
  Given a fake endpoint answering with cost "0.0001"
  When poll_decisions takes the finished job
  Then add_spend runs on the poll thread, decision_made (path, member, provider, latency_ms, cached,
    cost_usd) is emitted, and the mapped outcome is stored in the per-path cache

Scenario: Gate hit
  Given an unchanged redacted input
  Then start_decision spawns nothing and the cached answer stands
```

**Tests (unit)**: the five scenarios plus event serialization. **Tests (integration, tmux-gated)**: one `tick()` smoke test with `provider: jev` and a fake endpoint, confirming `poll_decisions` runs without error.

**Validation filter**: `cargo test --lib decisions` and `cargo test --lib events`

**Risks**: `telemetry_db` is not `Sync`; record spend when the result is taken, never on the worker. `decision_fallback.reason` takes only the enum label, never raw content.

---

## JEV-07: Triage manager and architect inbox messages

**Priority**: High | **Size**: M | **Depends on**: JEV-06 | **Parallel with**: JEV-08, JEV-09, JEV-10, JEV-11

**Files**
```
├── src/team/delivery/routing.rs orchestrator_only_reason (761-784) - OrchestratorOnlyReason::JevSuppressed
│     (labels "jev_noise" / "jev_status_reply"); role check via uses_management_batching (699-707)
├── routing.rs deliver_message_with_id (1104) - cache miss: heuristic now, start job for next time
└── routing.rs deliver_inbox_messages (1388) - cache miss: start job, skip message until answer or timeout
```

**Acceptance Criteria**
```gherkin
Scenario: Noise suppressed
  Given triage is enabled and decisions.triage holds Noise for hash(redact(body))
  When a message is delivered to the manager
  Then orchestrator_only_reason returns Some(JevSuppressed) and record_orchestrator_only_message logs it

Scenario: Escalation prioritized
  Given the cached verdict is Escalation
  Then the message is delivered via QueueTier::Priority when tiered inboxes are on, else prefixed "[escalation] "

Scenario: Engineers unaffected
  Given an engineer recipient
  Then is_engineer_ambient_chatter and today's checks decide, and no triage job starts

Scenario: Cache miss
  Given no cached verdict
  Then the live path behaves as today and starts one job
  And the inbox path defers the message until the answer arrives or 2000 ms pass, then uses the heuristic
```

**Tests (unit)**: the four scenarios, following the routing tests at 3955, 4074 and 4107-4157.

**Validation filter**: `cargo test --lib routing`

**Risks**: The inbox deferral is a PRP high-risk area. A stuck job must never hold a message past the triage timeout. Orchestrator previews (842-860) must use the redacted body.

---

## JEV-08: Add the completion veto

**Priority**: High | **Size**: M | **Depends on**: JEV-06 (and JEV-05 for completion_questions) | **Parallel with**: JEV-07, JEV-09, JEV-10, JEV-11

**Files**
```
├── src/team/daemon/health/poll_shim/shim_events.rs on_completion - store redact(response) in
│     decisions.pending_response before merge::handle_engineer_completion (303)
├── src/team/merge/completion/mod.rs - start the decision beside the verification job (143-146);
│     veto in finish_engineer_completion between 172 and 183
├── src/team/merge/completion/verify_job.rs apply_verification_result - join the veto, up to 10000 ms
└── src/team/merge/completion/gates.rs reject_completion - explicit Rejection override via a thin wrapper
```

**Acceptance Criteria**
```gherkin
Scenario: Veto a passing completion
  Given has_required_evidence && passed and a CompletionOutcome with narration_only == true
  Then reject_completion receives Rejection::NarrationOnly and completion_vetoed (task, reason) is emitted
  And done == false without narration gives Rejection::InsufficientEvidence

Scenario: Hard gates untouched
  Given a completion rejected as NoCommits or TestsFailed
  Then no veto is applied and Jev never clears the rejection

Scenario: Fallback or timeout
  Given the decision falls back or is still pending after 10000 ms
  Then the completion proceeds exactly as today
```

**Tests (unit)**: the three scenarios plus the test-skipped path deferring across ticks, using the gate fixtures (tests/gates.rs:250-330).

**Validation filter**: `cargo test --lib completion`

**Risks**: `Rejection::classify` yields `TestsFailed` when evidence is fine, so pass the explicit `Rejection`. `NarrationOnly` escalates after 2 rejections (gates.rs:287-291), as intended. A Jev error must never block a completion. This is the PRP's riskiest join.

---

## JEV-09: Send the advisory review rubric

**Priority**: Medium | **Size**: S | **Depends on**: JEV-06 | **Parallel with**: JEV-07, JEV-08, JEV-10, JEV-11

**Files**
```
└── src/team/merge/completion/decision.rs route_to_manual_review (170-200), route_override_to_review (134-166) -
      start the rubric decision when criteria exist; on finish queue a manager message and emit review_rubric
```

**Acceptance Criteria**
```gherkin
Scenario: Rubric message
  Given rubric is enabled and task #7 has two acceptance criteria
  When the rubric job finishes
  Then the manager receives "[rubric] Task #7: criteria met: yes|no" with one "- <criterion>: <score label>" line each
  And review_rubric (task, criteria_met, scores) is emitted

Scenario: No criteria
  Then no job starts

Scenario: Failure
  Given the rubric decision falls back
  Then no message is sent and one decision_fallback is emitted
```

**Tests (unit)**: the three scenarios plus "merge and bench decisions are unchanged".

**Validation filter**: `cargo test --lib rubric` and `cargo test --lib completion`

**Risks**: The rubric is advisory. It must never feed `decide_merge_route` or the bench bar.

---

## JEV-10: Classify supervisor screen state

**Priority**: Medium | **Size**: M | **Depends on**: JEV-06 | **Parallel with**: JEV-07, JEV-08, JEV-09, JEV-11

**Files**
```
├── src/team/daemon/agent_handle.rs - pty_runtime: bool set at spawn (launcher.rs 189/392, shim_spawn.rs 63-103)
└── src/team/daemon/health/poll_shim.rs - send Command::CaptureScreen{last_n_lines: Some(100)} once per poll
      interval; handle Event::ScreenCapture at 306-309 (redact, never log content)
```

**Acceptance Criteria**
```gherkin
Scenario: Capture only for PTY supervisors
  Given a manager with pty_runtime == true and an SDK-mode architect and an engineer
  Then CaptureScreen is sent only to the manager

Scenario: Verdict updates the daemon view
  Given a ScreenOutcome::Idle that differs from the handle state
  Then apply_state_change and the MemberState mapping run as on_state_changed does
  And the shim's own state machine is unchanged

Scenario: Content never logged
  Given a ScreenCapture containing a planted secret
  Then the content is absent from the shim event log and tracing output
```

**Tests (unit)**: the three scenarios with socketpair mocks (`insert_mock_handle`, 1393-1409). **Tests (integration, tmux-gated)**: one smoke test spawning a PTY manager and confirming a capture round trip.

**Validation filter**: `cargo test --lib poll_shim`

**Risks**: Compute PTY-runtime as `!(agent_supports_sdk_mode(agent) && team_config.use_sdk_mode)`. `debug!(event = ?event)` at poll_shim.rs:307 would dump content; skip it for `ScreenCapture`.

---

## JEV-11: Add Jev stall reasons

**Priority**: Medium | **Size**: S | **Depends on**: JEV-06 | **Parallel with**: JEV-07, JEV-08, JEV-09, JEV-10

**Files**
```
└── src/team/daemon/health/stall.rs record_supervisory_stall_reason (572-622) - on SupervisoryProgress::None
      start a stall decision on redacted screen/output; put the reason in event.details
```

**Acceptance Criteria**
```gherkin
Scenario: Reason added
  Given supervisory_progress_signal returns SupervisoryProgress::None and the StallOutcome is stuck with reason "looping"
  Then stall_detected_with_reason carries "looping" in details and the stall summary includes it

Scenario: Heuristic signals untouched
  Given the signal is Expected or Incidental
  Then no stall job starts and the output is unchanged

Scenario: Not stuck or fallback
  Given stuck == false or a fallback
  Then today's output stands and no stall is suppressed
```

**Tests (unit)**: the three scenarios, following daemon/tests.rs:2960-2996.

**Validation filter**: `cargo test --lib stall`

**Risks**: Reasons are `&'static str`, so the Jev text goes in `details`. A Jev "not stalled" answer never suppresses a stall.

---

## JEV-12: Document the feature

**Priority**: High | **Size**: S | **Depends on**: JEV-01 to JEV-11 (content), can be drafted earlier

**Files**
```
├── docs/config-reference.md - decisions bullet, field list, privacy note (what is sent, redaction, zero data retention)
├── docs/migrating-to-0.12.md - opt-in feature
└── CHANGELOG.md - Added: Jev decisions and the five new events
```

**Acceptance Criteria**
```gherkin
Scenario: Config documented
  Given docs/config-reference.md
  Then every DecisionsConfig field appears with its default and the privacy note names the redaction kinds

Scenario: Opt-in stated
  Given docs/migrating-to-0.12.md
  Then it says provider defaults to heuristic and shows provider: jev with api_key_env
```

**Validation**: the shared commands plus the full `cargo test --lib`. Never hand-edit `docs/reference/cli.md`.

---

## Dependency Graph and Critical Path

```text
JEV-01 -> JEV-02 -> JEV-03 -> JEV-04 -> JEV-05 -> JEV-06 -+-> JEV-07 (triage) ------+
                                                          +-> JEV-08 (completion) --+
                                                          +-> JEV-09 (rubric) ------+-> JEV-12 (docs)
                                                          +-> JEV-10 (screen) ------+
                                                          +-> JEV-11 (stall) -------+

JEV-08 also depends directly on JEV-05 (completion_questions and CompletionOutcome).
```

- **Critical path**: JEV-01, 02, 03, 04, 05, 06 in strict order. Then JEV-07 to JEV-11, each after JEV-06. JEV-12 lands last.
- **Longest branch**: JEV-08 (veto join and test-skipped path). Plan it to finish last among the path tasks.
- **Parallel after JEV-06**: JEV-07 to JEV-11. Each is independently switchable through `paths`.
- **Conflict hot spots**: JEV-08 and JEV-10 both touch `poll_shim` code (`shim_events.rs` and `poll_shim.rs`). JEV-08 and JEV-09 both touch `merge/completion/`.
- **Bottlenecks**: JEV-08, JEV-07 (inbox deferral) and JEV-10 (daemon-side screen state beside the shim's state).

## Implementation Recommendations

- **Recommended path order**: build JEV-01 to JEV-06 first. Then add one path at a time: triage (JEV-07), completion (JEV-08), rubric (JEV-09), stall (JEV-11), screen (JEV-10).
- **Team**: two engineers.
  - A: JEV-01 to JEV-06, then JEV-08 and JEV-10.
  - B: drafts JEV-12, then JEV-07, JEV-09 and JEV-11 after JEV-06.
- **Sequencing**: merge JEV-01 early so the builder default protects the existing suite. Run the full `cargo test --lib` on every PR.
- **Manual check before release**: on a scratch project with a real key and `provider: jev`:
  1. Send a noise message to the manager. Expect suppression and `decision_made`.
  2. Complete a narration-only task. Expect `completion_vetoed`.
  3. Check `decision_spend` in `telemetry.db`.
  4. Unset the key. Expect one `decision_fallback` with reason `no_key`.
